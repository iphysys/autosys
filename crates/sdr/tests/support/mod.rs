//! Shared fixtures: a manager over testkit's in-memory repository, and
//! builders for transitions and their operations.

#![allow(
    clippy::expect_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test binary uses every helper"
)]

use std::sync::Arc;

use contracts::ExecutionContext;
use contracts::ids::ProjectId;
use contracts::types::{HarnessVersions, Operation, Transition};
use sdr::{CommitInfo, InitialVersion, SdrManager, Validation};
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::clock::ManualClock;
use testkit::stores::InMemorySdrRepository;

pub struct Fixture {
    pub manager: SdrManager,
    pub repository: Arc<InMemorySdrRepository>,
    pub clock: Arc<ManualClock>,
    pub project: ProjectId,
    pub ctx: ExecutionContext,
}

/// A manager whose project `p1` has version 0.
pub async fn fixture() -> Fixture {
    let repository = Arc::new(InMemorySdrRepository::new());
    let clock = Arc::new(ManualClock::at_epoch());
    let manager =
        SdrManager::new(repository.clone(), clock.clone()).expect("the embedded schemas compile");
    let project: ProjectId = id("p1");
    let ctx = ExecutionContext::new(id("corr-sdr"));
    manager
        .create_project(&project, initial(), &ctx)
        .await
        .expect("create the project");
    Fixture {
        manager,
        repository,
        clock,
        project,
        ctx,
    }
}

pub fn harness() -> HarnessVersions {
    parse(json!({
        "harness_build": "test-build",
        "model_ids": ["test-model"],
        "prompts": [],
        "skills": [],
        "agent_specs": []
    }))
}

pub fn initial() -> InitialVersion {
    InitialVersion {
        transition_id: id("tr-init"),
        actor: parse(json!({ "kind": "system", "component": "test" })),
        reason: parse(json!("Start the design.")),
        harness: harness(),
    }
}

pub fn commit_info(evidence: &[&str]) -> CommitInfo {
    CommitInfo {
        evidence_ids: evidence.iter().map(|e| id(e)).collect(),
        harness: harness(),
    }
}

pub fn transition(
    transition_id: &str,
    base_version: u64,
    operations: Vec<Operation>,
) -> Transition {
    let mut transition: Transition = parse(json!({
        "schema_version": "0.1",
        "id": transition_id,
        "base_version": base_version,
        "actor": { "kind": "agent", "agent_id": "test_agent" },
        "reason": "Change the design.",
        "final": false,
        "operations": [{ "op": "link_evidence", "element_id": "x", "evidence_id": "x" }]
    }));
    transition.operations = operations;
    transition
}

pub fn op(value: Value) -> Operation {
    parse(value)
}

pub fn add_requirement(element_id: &str) -> Operation {
    op(json!({
        "op": "add_requirement",
        "requirement": {
            "id": element_id,
            "text": "Mass at most 1 kg.",
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": 1, "unit": "kg" } }
            },
            "evidence_ids": []
        }
    }))
}

pub fn set_parameter(element_id: &str, value: f64) -> Operation {
    op(json!({
        "op": "set_parameter",
        "parameter": {
            "id": element_id,
            "value": { "value": value, "unit": "kg" },
            "source": { "kind": "user" },
            "evidence_ids": []
        }
    }))
}

pub fn add_decision(element_id: &str, related: &[&str]) -> Operation {
    op(json!({
        "op": "add_decision",
        "decision": {
            "id": element_id,
            "choice": "Use a ribbed section.",
            "rationale": "Keeps the wall thickness at lower mass.",
            "related_ids": related
        }
    }))
}

pub fn add_assumption(element_id: &str) -> Operation {
    op(json!({
        "op": "add_assumption",
        "assumption": {
            "id": element_id,
            "statement": "The part is aluminium 6061.",
            "related_ids": []
        }
    }))
}

pub fn link_evidence(element_id: &str, evidence_id: &str) -> Operation {
    op(json!({ "op": "link_evidence", "element_id": element_id, "evidence_id": evidence_id }))
}

impl Fixture {
    /// Proposes and validates `transition` on this fixture's project.
    pub async fn validate(&self, transition: Transition) -> Validation {
        let proposed = self.manager.propose(self.project.clone(), transition);
        self.manager
            .validate(proposed, &self.ctx)
            .await
            .expect("validation runs")
    }
}
