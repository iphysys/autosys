//! Shared set-up for the model gateway tests.

#![allow(dead_code, reason = "each test binary uses part of this module")]

pub mod replay;

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use contracts::types::{AgentSpec, ModelRetryPolicy, ModelRouting};
use contracts::{ContentBlock, ExecutionContext, Message, ModelRequest, Role, ToolSpec};
use model_gateway::Routing;
use model_gateway::Secret;
use model_gateway::providers::{AnthropicSettings, OpenAiCompatibleSettings};
use serde_json::{Value, json};
use testkit::builders::{id, parse};

pub const ANTHROPIC_TEST_KEY: &str = "sk-ant-test-key-never-logged";
pub const OPENAI_TEST_KEY: &str = "pliny-test-key-never-logged";

pub fn ctx() -> ExecutionContext {
    ExecutionContext::new(id("corr-model-gateway"))
}

/// Short waits, so retry tests stay fast.
pub fn retry(max_attempts: u64) -> ModelRetryPolicy {
    ModelRetryPolicy {
        max_attempts: NonZeroU64::new(max_attempts).expect("non-zero"),
        initial_backoff_ms: 1,
        max_backoff_ms: 10,
    }
}

pub fn anthropic_settings(base_url: &str) -> AnthropicSettings {
    AnthropicSettings {
        base_url: base_url.to_owned(),
        api_key: Secret::new(ANTHROPIC_TEST_KEY),
        workspace_id: None,
        effort: None,
        refusal_fallback: false,
        headers: Vec::new(),
        timeout: Duration::from_secs(5),
        retry: retry(3),
        ca_bundle: None,
    }
}

pub fn openai_settings(base_url: &str) -> OpenAiCompatibleSettings {
    OpenAiCompatibleSettings {
        base_url: base_url.to_owned(),
        base_url_origin: "PLINY_BASE_URL".to_owned(),
        api_key: Secret::new(OPENAI_TEST_KEY),
        headers: Vec::new(),
        timeout: Duration::from_secs(5),
        retry: retry(3),
        ca_bundle: None,
    }
}

/// A request for `model` with one user message and the `create_plate` tool.
pub fn request(model: &str, text: &str) -> ModelRequest {
    ModelRequest {
        model_id: model.to_owned(),
        system: "You design mechanical parts.".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }],
        tools: vec![ToolSpec {
            name: id("create_plate"),
            description: "Builds a rectangular plate.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": { "length_mm": { "type": "number" } },
                "required": ["length_mm"]
            }),
        }],
        max_output_tokens: 1024,
    }
}

/// `config/models/routing.yaml` as committed.
pub fn committed_routing() -> Routing {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/models/routing.yaml");
    let text = std::fs::read_to_string(&path).expect("routing.yaml");
    let config: ModelRouting = serde_saphyr::from_str(&text).expect("routing.yaml parses");
    Routing::new(config).expect("routing.yaml is consistent")
}

/// A routing configuration from JSON, checked.
pub fn routing(value: Value) -> Result<Routing, contracts::Error> {
    Routing::new(parse(value))
}

/// An agent spec; `model_profile` is omitted when `None`.
pub fn agent(id: &str, model_profile: Option<&str>, tools: &[&str]) -> AgentSpec {
    let mut spec = json!({
        "schema_version": "0.1",
        "id": id,
        "version": "0.1.0",
        "kind": "agent",
        "implementation": "bounded_worker",
        "name": "Test agent",
        "description": "An agent for model gateway tests.",
        "role": "Tester",
        "goal": "Exercise profile resolution.",
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

pub fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
    vars.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

pub fn test_ca_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/test-ca.pem")
        .display()
        .to_string()
}
