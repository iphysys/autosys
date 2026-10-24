//! Shared fixtures for the runtime's tests: the committed configuration and
//! prompts, and task builders.
//!
//! Tests here may not name the SDR repository trait (arch-check scans test
//! files); an `SdrManager` gets testkit's `sdr_storage()`. Test requirements
//! are fixtures written here, never read from `evals/`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test binary uses every helper"
)]

use std::fs;
use std::path::{Path, PathBuf};

use context::PromptLibrary;
use contracts::types::{AgentSpec, Task, ToolOverlay};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use testkit::builders::parse;

pub const AGENT: &str = "main_orchestrator";
pub const PROJECT: &str = "p1";

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

/// A task for the orchestrator. `budget` and `requirement_ids` are JSON;
/// everything the task may read and write is the whole design.
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

/// A budget as JSON, within the orchestrator's limits.
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

/// The default test budget: roomy everywhere, three repairs.
pub fn roomy_budget() -> Value {
    budget(200_000, 20, 20, 600, 3)
}

pub fn task() -> Task {
    parse(task_json(&["R-mass"], roomy_budget()))
}
