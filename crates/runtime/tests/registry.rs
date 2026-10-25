//! The agent registry refuses a spec the runtime could not run, and the
//! factory refuses a task that asks for more than its agent allows.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use contracts::ErrorCode;
use contracts::types::{AgentSpec, AgentSpecOnRepairExhausted, Task};
use runtime::{AgentRegistry, RuntimeError};
use serde_json::{Value, json};
use support::*;
use testkit::builders::parse;

fn registry_of(specs: Vec<AgentSpec>) -> Result<AgentRegistry, RuntimeError> {
    let tools = overlay_tools();
    let names: Vec<&str> = tools.iter().map(String::as_str).collect();
    AgentRegistry::new(specs, &prompts(), &names)
}

fn problems(specs: Vec<AgentSpec>) -> Vec<String> {
    match registry_of(specs) {
        Err(RuntimeError::InvalidAgents { problems }) => problems,
        Err(other) => panic!("expected InvalidAgents, got {other}"),
        Ok(_) => panic!("the specs were accepted"),
    }
}

/// The committed spec with `change` applied to its JSON.
fn spec_with(change: impl FnOnce(&mut Value)) -> AgentSpec {
    let mut json = serde_json::to_value(orchestrator_spec()).expect("serialize");
    change(&mut json);
    parse(json)
}

#[test]
fn the_committed_spec_is_accepted_with_its_provenance() {
    let registry = registry_of(vec![orchestrator_spec()]).expect("the committed spec registers");
    assert_eq!(registry.ids().collect::<Vec<_>>(), [AGENT]);
    let agent = registry.get(AGENT).expect("registered");
    let provenance = agent.provenance();
    assert_eq!(provenance.id.as_str(), AGENT);
    assert_eq!(
        provenance.version.as_ref().map(|v| v.as_str()),
        Some("0.1.0")
    );
    assert!(provenance.sha256.as_str().starts_with("sha256:"));

    // The same spec always hashes the same; a changed spec does not.
    let again = registry_of(vec![orchestrator_spec()]).expect("registers");
    assert_eq!(again.get(AGENT).expect("agent").provenance(), provenance);
    let changed =
        registry_of(vec![spec_with(|s| s["goal"] = json!("Another goal."))]).expect("registers");
    assert_ne!(
        changed.get(AGENT).expect("agent").provenance().sha256,
        provenance.sha256
    );
    assert_eq!(registry.specs(), vec![orchestrator_spec()]);
}

#[test]
fn a_prompt_that_is_not_in_the_library_is_refused_with_the_alternatives() {
    let found = problems(vec![spec_with(|s| {
        s["prompt"] = json!("orchestrator/missing.md")
    })]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with(
            "agent `main_orchestrator`: prompt: unknown prompt `orchestrator/missing.md`"
        ),
        "{found:?}"
    );
    assert!(
        found[0].contains("available: orchestrator/main.md, system/base.md"),
        "{found:?}"
    );
}

#[test]
fn a_tool_nobody_offers_is_refused_with_the_alternatives() {
    let found = problems(vec![spec_with(|s| {
        s["tools"] = json!(["sdr_read", "export_part"])
    })]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("tool `export_part` is not available"),
        "{found:?}"
    );
    assert!(
        found[0].contains("available: create_plate, measure, sdr_propose, sdr_read"),
        "{found:?}"
    );
}

#[test]
fn skills_are_refused_until_a_skill_registry_exists() {
    let found = problems(vec![spec_with(|s| {
        s["skills"] = json!(["cad_construction"])
    })]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("cad_construction"), "{found:?}");
    assert!(found[0].contains("no skill registry"), "{found:?}");
}

#[test]
fn escalation_is_refused_and_names_increment_3() {
    let spec = spec_with(|s| s["on_repair_exhausted"] = json!("escalate"));
    assert_eq!(
        spec.on_repair_exhausted,
        AgentSpecOnRepairExhausted::Escalate
    );
    let found = problems(vec![spec]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("escalate"), "{found:?}");
    assert!(found[0].contains("Increment 3"), "{found:?}");
    assert!(found[0].contains("use fail"), "{found:?}");
}

#[test]
fn a_result_schema_that_does_not_compile_is_refused() {
    let found = problems(vec![spec_with(|s| {
        s["result_schema"] = json!("results/missing.schema.json")
    })]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("result_schema: results/missing.schema.json is not a schema"),
        "{found:?}"
    );
}

#[test]
fn duplicate_ids_are_refused_and_every_problem_is_listed() {
    let broken = spec_with(|s| {
        s["id"] = json!("broken");
        s["prompt"] = json!("agents/broken.md");
        s["skills"] = json!(["x"]);
    });
    let found = problems(vec![orchestrator_spec(), orchestrator_spec(), broken]);
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(
        found[0],
        "agent `main_orchestrator` is defined more than once"
    );
    assert!(found[1].starts_with("agent `broken`: prompt:"), "{found:?}");
    assert!(found[2].starts_with("agent `broken`: skills"), "{found:?}");
}

#[test]
fn an_unknown_agent_lists_the_registered_ones() {
    let registry = registry_of(vec![orchestrator_spec()]).expect("registers");
    let Err(error) = registry.get("specialist") else {
        panic!("an unknown agent was found");
    };
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(
        error.to_string(),
        "unknown agent `specialist`; available: main_orchestrator"
    );
}

fn task_problems(task: &Task) -> Vec<String> {
    let registry = registry_of(vec![orchestrator_spec()]).expect("registers");
    match registry.agent_for(task) {
        Err(RuntimeError::InvalidTask { task: id, problems }) => {
            assert_eq!(id, task.id.as_str());
            problems
        }
        Err(other) => panic!("expected InvalidTask, got {other}"),
        Ok(_) => panic!("the task was accepted"),
    }
}

#[test]
fn a_task_within_the_agent_is_accepted() {
    let registry = registry_of(vec![orchestrator_spec()]).expect("registers");
    let at_the_limits: Task = parse(task_json(&["R-mass"], budget(1_000_000, 30, 40, 600, 3)));
    let agent = registry.agent_for(&at_the_limits).expect("fits");
    assert_eq!(agent.spec().id.as_str(), AGENT);
    registry.agent_for(&task()).expect("a narrower task fits");
}

#[test]
fn a_task_above_the_agents_limits_is_refused_field_by_field() {
    let task: Task = parse(task_json(&["R-mass"], budget(2_000_000, 30, 41, 600, 4)));
    let found = task_problems(&task);
    assert_eq!(
        found,
        [
            "budget max_tokens 2000000 exceeds agent `main_orchestrator`'s limit 1000000",
            "budget max_steps 41 exceeds agent `main_orchestrator`'s limit 40",
            "budget max_repair_attempts 4 exceeds agent `main_orchestrator`'s limit 3",
        ]
    );
}

#[test]
fn a_task_cannot_widen_the_agents_tools_or_skills() {
    let mut json = task_json(&["R-mass"], roomy_budget());
    json["tools"] = json!(["measure", "export_part"]);
    json["skills"] = json!(["cad_construction"]);
    let found = task_problems(&parse(json));
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found[0].starts_with("tool `export_part` is not one of agent `main_orchestrator`'s tools"),
        "{found:?}"
    );
    assert!(found[1].contains("skill `cad_construction`"), "{found:?}");
}

#[test]
fn a_task_that_breaks_its_schema_or_names_no_known_agent_is_refused() {
    // The Rust type accepts an empty list; the schema's minItems does not.
    let mut json = task_json(&["R-mass"], roomy_budget());
    json["acceptance_criteria"] = json!([]);
    let found = task_problems(&parse(json));
    assert!(
        found.iter().any(|p| p.starts_with("/acceptance_criteria")),
        "{found:?}"
    );

    let mut json = task_json(&["R-mass"], roomy_budget());
    json["agent_id"] = json!("specialist");
    let registry = registry_of(vec![orchestrator_spec()]).expect("registers");
    let Err(error) = registry.agent_for(&parse(json)) else {
        panic!("a task for an unknown agent was accepted");
    };
    assert!(
        matches!(error, RuntimeError::UnknownAgent { .. }),
        "{error}"
    );
}
