//! Valid instances for tests. Built from JSON so the generated types'
//! deserializers check every value, as they would for real data.

use chrono::{DateTime, Utc};
use contracts::types::{
    AgentSpec, ArtifactMetadata, CheckpointRef, Event, Evidence, Job, JobStatus, SdrDocument,
    SdrVersionRecord, StepOutcome, Timestamp, Transition,
};
use contracts::{ContentBlock, Message, ModelRequest, NewEvent, NewStep, Role, ToolRequest};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Deserializes `value` into `T`, panicking with the reason if it is invalid.
pub fn parse<T: DeserializeOwned>(value: Value) -> T {
    let text = value.to_string();
    serde_json::from_value(value).unwrap_or_else(|e| panic!("invalid test value {text}: {e}"))
}

/// Any generated identifier newtype, for example `id::<RunId>("run-1")`.
pub fn id<T: DeserializeOwned>(value: &str) -> T {
    parse(json!(value))
}

pub fn timestamp(at: DateTime<Utc>) -> Timestamp {
    Timestamp(at)
}

/// `sha256:` followed by the lowercase hex digest of `content`.
pub fn sha256_of(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

fn task(task_id: &str) -> Value {
    json!({
        "schema_version": "0.1",
        "id": task_id,
        "project_id": "test-project",
        "agent_id": "test_agent",
        "name": "Test task",
        "description": "A task built by testkit.",
        "goal": "Exercise a contract.",
        "acceptance_criteria": ["The contract holds."],
        "requirement_ids": [],
        "skills": [],
        "tools": [],
        "sdr": {
            "base_version": 0,
            "read_slice": { "kinds": [], "ids": [] },
            "write_scope": { "kinds": [], "ids": [] }
        },
        "budget": {
            "max_tokens": 1000,
            "max_tool_calls": 10,
            "max_steps": 10,
            "max_wall_time_seconds": 60,
            "max_repair_attempts": 1
        },
        "result_schema": "results/result.schema.json"
    })
}

/// A queued job without a lease, as `JobStore::insert` expects.
pub fn job(job_id: &str, run_id: &str, created_at: DateTime<Utc>) -> Job {
    let at = rfc3339(created_at);
    parse(json!({
        "schema_version": "0.2",
        "id": job_id,
        "run_id": run_id,
        "project_id": "test-project",
        "status": { "state": "queued" },
        "task": task(&format!("task-{job_id}")),
        "idempotency_key": job_id,
        "attempt": 0,
        "created_at": at,
        "updated_at": at,
        "sdr_base_version": 0,
        "progress": { "steps_completed": 0, "last_event_seq": 0 },
        "pending_work": [],
        "artifact_ids": [],
        "budget_used": {
            "input_tokens": 0,
            "output_tokens": 0,
            "tool_calls": 0,
            "steps": 0,
            "wall_time_seconds": 0
        }
    }))
}

/// A reference to checkpoint `version`, with a hash made from the version.
pub fn checkpoint(version: u64) -> CheckpointRef {
    parse(json!({
        "version": version,
        "sha256": sha256_of(format!("checkpoint {version}").as_bytes())
    }))
}

/// Parked until `approval_id` is answered, on checkpoint `version`.
pub fn suspended(approval_id: &str, version: u64) -> JobStatus {
    parse(json!({
        "state": "suspended",
        "wake_condition": { "kind": "approval_answered", "approval_id": approval_id },
        "checkpoint": checkpoint(version)
    }))
}

/// Completed with `stop_reason` (`success`, `failure` or
/// `budget_exhausted`), on checkpoint `version`.
pub fn completed(stop_reason: &str, version: u64) -> JobStatus {
    parse(json!({
        "state": "completed",
        "stop_reason": stop_reason,
        "checkpoint": checkpoint(version)
    }))
}

/// An event to append, with the given payload JSON.
pub fn new_event(run_id: &str, occurred_at: DateTime<Utc>, payload: Value) -> NewEvent {
    NewEvent {
        run_id: id(run_id),
        occurred_at: timestamp(occurred_at),
        correlation_id: id("corr-test"),
        actor: parse(json!({ "kind": "system", "component": "testkit" })),
        payload: parse(payload),
    }
}

/// A small event payload whose `reason` makes each event distinguishable.
pub fn marker_payload(marker: usize) -> Value {
    json!({ "type": "cancel_requested", "reason": format!("marker {marker}") })
}

/// One payload of every event type.
pub fn every_event_payload() -> Vec<Value> {
    let error = json!({ "code": "internal", "message": "test", "retryable": false });
    vec![
        json!({ "type": "run_started", "task_id": "task-1", "agent_id": "test_agent" }),
        json!({ "type": "run_stopped", "stop_reason": "success", "sdr_version": 1 }),
        json!({ "type": "job_state_changed", "from": "queued", "to": { "state": "running" } }),
        json!({
            "type": "context_assembled",
            "context_hash": sha256_of(b"context"),
            "total_tokens": 10,
            "sdr_version": 1,
            "last_event_seq": 2
        }),
        json!({
            "type": "model_call_completed",
            "model_profile": "test_profile",
            "model_id": "test-model",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }),
        json!({
            "type": "tool_call_started",
            "step_id": "step-1",
            "tool": "test_tool",
            "idempotency_key": "run-1:step-1"
        }),
        json!({
            "type": "tool_call_completed",
            "step_id": "step-1",
            "tool": "test_tool",
            "outcome": "error",
            "error": error,
            "evidence_ids": [],
            "duration_ms": 5
        }),
        json!({
            "type": "check_completed",
            "check_id": "test_check",
            "evidence_id": "ev-1",
            "verdict": "pass"
        }),
        json!({
            "type": "gate_decided",
            "gate": "test_gate",
            "subject_id": "step-1",
            "decision": "allow",
            "reasons": [],
            "evidence_ids": []
        }),
        json!({
            "type": "approval_requested",
            "approval_id": "appr-1",
            "gate": "test_gate",
            "subject_id": "tr-1",
            "summary": "Approve?"
        }),
        json!({ "type": "approval_answered", "approval_id": "appr-1", "answer": "approve" }),
        json!({
            "type": "sdr_transition",
            "transition_id": "tr-1",
            "outcome": "committed",
            "base_version": 0,
            "new_version": 1,
            "reasons": []
        }),
        json!({
            "type": "artifact_published",
            "artifact_id": "art-1",
            "format": "step",
            "source_sdr_version": 1
        }),
        json!({ "type": "cancel_requested" }),
        json!({ "type": "error", "error": error }),
    ]
}

/// One stored event of every type, with sequence numbers 1, 2, …
pub fn every_event(run_id: &str, at: DateTime<Utc>) -> Vec<Event> {
    every_event_payload()
        .into_iter()
        .enumerate()
        .map(|(i, payload)| {
            parse(json!({
                "schema_version": "0.1",
                "run_id": run_id,
                "seq": i + 1,
                "occurred_at": rfc3339(at),
                "correlation_id": "corr-test",
                "actor": { "kind": "system", "component": "testkit" },
                "payload": payload
            }))
        })
        .collect()
}

/// An SDR document holding one parameter whose value records `version`, so
/// documents of different versions differ.
pub fn sdr_document(project_id: &str, version: u64, parent: Option<u64>) -> SdrDocument {
    let mut doc = json!({
        "schema_version": "0.1",
        "project_id": project_id,
        "version": version,
        "requirements": [{
            "id": "req-mass",
            "text": "Mass at most 1 kg.",
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": 1, "unit": "kg" } }
            },
            "evidence_ids": []
        }],
        "design": {
            "parameters": [{
                "id": "part.mass",
                "value": { "value": version, "unit": "kg" },
                "source": { "kind": "user" },
                "evidence_ids": []
            }],
            "decisions": [],
            "assumptions": []
        }
    });
    if let Some(parent) = parent {
        doc["parent_version"] = json!(parent);
    }
    parse(doc)
}

/// The version record matching [`sdr_document`].
pub fn version_record(
    project_id: &str,
    version: u64,
    parent: Option<u64>,
    committed_at: DateTime<Utc>,
) -> SdrVersionRecord {
    let mut record = json!({
        "schema_version": "0.1",
        "project_id": project_id,
        "version": version,
        "transition_id": format!("tr-{version}"),
        "actor": { "kind": "system", "component": "testkit" },
        "reason": format!("Commit version {version}."),
        "committed_at": rfc3339(committed_at),
        "evidence_ids": [],
        "document_sha256": sha256_of(format!("{project_id}/{version}").as_bytes()),
        "harness": {
            "harness_build": "testkit",
            "model_ids": [],
            "prompts": [],
            "skills": [],
            "agent_specs": []
        }
    });
    if let Some(parent) = parent {
        record["parent_version"] = json!(parent);
    }
    parse(record)
}

/// A transition that sets `part.mass` against `base_version`.
pub fn transition(transition_id: &str, base_version: u64, is_final: bool) -> Transition {
    parse(json!({
        "schema_version": "0.1",
        "id": transition_id,
        "base_version": base_version,
        "actor": { "kind": "agent", "agent_id": "test_agent" },
        "reason": "Set the mass.",
        "final": is_final,
        "operations": [{
            "op": "set_parameter",
            "parameter": {
                "id": "part.mass",
                "value": { "value": 0.5, "unit": "kg" },
                "source": { "kind": "user" },
                "evidence_ids": []
            }
        }]
    }))
}

/// A tool request with the given JSON object as arguments.
pub fn tool_request(tool: &str, step_id: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool: id(tool),
        step_id: id(step_id),
        idempotency_key: id(&format!("run-1:{step_id}")),
        arguments: arguments
            .as_object()
            .cloned()
            .expect("tool arguments must be a JSON object"),
    }
}

/// A step about to run in run `run-1`, keyed `run-1:<step_id>`. `request`
/// stands for the request's content; equal values give equal hashes.
pub fn new_step(step_id: &str, tool: &str, request: &str) -> NewStep {
    NewStep {
        idempotency_key: id(&format!("run-1:{step_id}")),
        run_id: id("run-1"),
        step_id: id(step_id),
        tool: id(tool),
        request_sha256: id(&sha256_of(request.as_bytes())),
    }
}

/// A succeeded step outcome with `result` and one evidence id.
pub fn succeeded(result: Value) -> StepOutcome {
    parse(json!({
        "kind": "succeeded",
        "tool_version": "0.1.0",
        "result": result,
        "evidence_ids": ["ev-step-1"]
    }))
}

/// A failed step outcome with the given error code.
pub fn failed(code: &str) -> StepOutcome {
    parse(json!({
        "kind": "failed",
        "error": { "code": code, "message": format!("the step failed: {code}"), "retryable": false }
    }))
}

/// A model request with one user message and no tools.
pub fn model_request(text: &str) -> ModelRequest {
    ModelRequest {
        model_id: "test-model".to_owned(),
        system: "You are a test.".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }],
        tools: Vec::new(),
        max_output_tokens: 100,
    }
}

/// An agent spec with the given tools; `model_profile` is left out when
/// `None`, so the agent gets the routing default.
pub fn agent_spec(id: &str, model_profile: Option<&str>, tools: &[&str]) -> AgentSpec {
    let mut spec = json!({
        "schema_version": "0.1",
        "id": id,
        "version": "0.1.0",
        "kind": "agent",
        "implementation": "bounded_worker",
        "name": "Test agent",
        "description": "An agent for tests.",
        "role": "Tester",
        "goal": "Be resolved.",
        "responsibilities": ["Be resolved."],
        "prompt": "agents/test.md",
        "result_schema": "results/result.schema.json",
        "skills": [],
        "tools": tools,
        "sdr": { "read_slice": "from_task", "write_scope": "from_task" },
        "limits": {
            "max_tokens": 1000,
            "max_tool_calls": 10,
            "max_steps": 10,
            "max_wall_time_seconds": 60,
            "max_repair_attempts": 1
        },
        "on_repair_exhausted": "fail"
    });
    if let Some(profile) = model_profile {
        spec["model_profile"] = json!(profile);
    }
    parse(spec)
}

/// A check evidence record; `verdict` is one of pass, fail, unverified, error.
pub fn evidence(id: &str, run_id: &str, created_at: DateTime<Utc>, verdict: &str) -> Evidence {
    parse(json!({
        "schema_version": "0.1",
        "id": id,
        "run_id": run_id,
        "created_at": rfc3339(created_at),
        "sdr_version": 1,
        "subject_ids": ["req-mass"],
        "body": {
            "kind": "check",
            "check_id": "test_check",
            "check_version": "0.1.0",
            "method": "deterministic",
            "inputs": [{ "name": "part.mass", "quantity": { "value": 0.5, "unit": "kg" } }],
            "verdict": verdict
        }
    }))
}

/// Metadata matching `content` (correct sha256 and size).
pub fn artifact(
    id: &str,
    run_id: &str,
    created_at: DateTime<Utc>,
    content: &[u8],
) -> ArtifactMetadata {
    parse(json!({
        "schema_version": "0.1",
        "id": id,
        "run_id": run_id,
        "project_id": "test-project",
        "format": "step",
        "media_type": "model/step",
        "sha256": sha256_of(content),
        "size_bytes": content.len(),
        "source_sdr_version": 1,
        "tool": "test_tool",
        "tool_version": "0.1.0",
        "step_id": "step-1",
        "evidence_ids": [],
        "created_at": rfc3339(created_at)
    }))
}
