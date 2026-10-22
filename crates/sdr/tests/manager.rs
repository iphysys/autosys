//! SdrManager behavior: schema validation beyond the Rust types, semantic
//! rules, version records, slices and document upgrades.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use contracts::ids::SdrVersion;
use contracts::types::{ElementKind, Operation, SdrSlice, Timestamp};
use contracts::{Clock, ErrorCode, SdrRepository};
use sdr::{InvalidTransition, ProblemKind, ValidatedTransition, Validation};
use serde_json::json;
use sha2::Digest;
use support::*;
use testkit::builders::{id, parse};

fn invalid(validation: Validation) -> InvalidTransition {
    match validation {
        Validation::Invalid(invalid) => invalid,
        Validation::Valid(v) => panic!("expected invalid, got valid {:?}", v.transition().id),
    }
}

fn valid(validation: Validation) -> ValidatedTransition {
    match validation {
        Validation::Valid(valid) => valid,
        Validation::Invalid(invalid) => panic!("expected valid, got {:?}", invalid.problems),
    }
}

#[tokio::test]
async fn an_empty_operation_list_breaks_min_items() {
    let f = fixture().await;
    // The Rust type accepts an empty Vec; the schema's minItems does not.
    let result = invalid(f.validate(transition("tr-1", 0, Vec::new())).await);
    assert!(result.has(ProblemKind::Schema), "{:?}", result.problems);
    assert!(
        result
            .problems
            .iter()
            .any(|p| p.message.starts_with("/operations")),
        "the problem names the field: {:?}",
        result.problems
    );
}

#[tokio::test]
async fn a_decision_without_related_ids_breaks_min_items() {
    let f = fixture().await;
    let ops = vec![add_requirement("req-1"), add_decision("dec-1", &[])];
    let result = invalid(f.validate(transition("tr-1", 0, ops)).await);
    assert!(result.has(ProblemKind::Schema), "{:?}", result.problems);
    assert!(
        result
            .problems
            .iter()
            .any(|p| p.message.starts_with("/operations/1/decision/related_ids")),
        "{:?}",
        result.problems
    );
}

#[tokio::test]
async fn non_finite_numbers_break_the_schema() {
    let f = fixture().await;
    for value in [f64::NAN, f64::INFINITY] {
        // JSON cannot carry these, so set them on the typed value.
        let mut operation = set_parameter("p", 1.0);
        if let Operation::SetParameter { parameter } = &mut operation {
            parameter.value.value = value;
        }
        let result = invalid(f.validate(transition("tr-1", 0, vec![operation])).await);
        assert!(
            result.has(ProblemKind::Schema),
            "{value}: {:?}",
            result.problems
        );
    }
}

#[tokio::test]
async fn semantic_rules_are_enforced() {
    let f = fixture().await;
    let cases: Vec<(Vec<Operation>, ProblemKind)> = vec![
        (
            vec![add_requirement("e1"), add_assumption("e1")],
            ProblemKind::DuplicateElement,
        ),
        (
            vec![add_requirement("e1"), set_parameter("e1", 1.0)],
            ProblemKind::DuplicateElement,
        ),
        (
            vec![link_evidence("missing", "ev-1")],
            ProblemKind::UnknownElement,
        ),
        (
            vec![add_assumption("a1"), link_evidence("a1", "ev-1")],
            ProblemKind::CannotHoldEvidence,
        ),
        (
            vec![add_decision("d1", &["missing"])],
            ProblemKind::UnknownElement,
        ),
        (
            vec![op(json!({
                "op": "set_parameter",
                "parameter": {
                    "id": "p1",
                    "value": { "value": 1, "unit": "kg" },
                    "source": { "kind": "assumption", "assumption_id": "missing" },
                    "evidence_ids": []
                }
            }))],
            ProblemKind::UnknownElement,
        ),
    ];
    for (ops, expected) in cases {
        let result = invalid(f.validate(transition("tr-1", 0, ops.clone())).await);
        assert!(result.has(expected), "{ops:?}: {:?}", result.problems);
    }
}

#[tokio::test]
async fn a_requirement_may_name_a_parameter_that_does_not_exist_yet() {
    let f = fixture().await;
    valid(
        f.validate(transition("tr-1", 0, vec![add_requirement("req-mass")]))
            .await,
    );
}

#[tokio::test]
async fn operations_apply_in_order() {
    let f = fixture().await;
    let ops = vec![
        add_requirement("req-mass"),
        set_parameter("part.mass", 2.0),
        set_parameter("part.mass", 0.5),
        add_assumption("asm-1"),
        add_decision("dec-1", &["req-mass", "part.mass"]),
        link_evidence("req-mass", "ev-1"),
        link_evidence("req-mass", "ev-1"),
        link_evidence("part.mass", "ev-2"),
    ];
    let validated = valid(f.validate(transition("tr-1", 0, ops)).await);
    let doc = validated.document();
    assert_eq!(doc.version, SdrVersion(1));
    assert_eq!(doc.parent_version, Some(SdrVersion(0)));
    assert_eq!(doc.design.parameters.len(), 1, "set_parameter replaces");
    assert_eq!(doc.design.parameters[0].value.value, 0.5);
    assert_eq!(
        doc.requirements[0].evidence_ids,
        vec![id("ev-1")],
        "linked once"
    );
    assert_eq!(doc.design.parameters[0].evidence_ids, vec![id("ev-2")]);
    assert_eq!(validated.base().version, SdrVersion(0));
}

#[tokio::test]
async fn a_commit_records_its_provenance() {
    let f = fixture().await;
    let validated = valid(
        f.validate(transition("tr-1", 0, vec![add_requirement("req-1")]))
            .await,
    );
    let expected = validated.document().clone();
    let record = f
        .manager
        .commit(validated, commit_info(&["ev-gate"]), &f.ctx)
        .await
        .expect("commit");

    assert_eq!(record.version, SdrVersion(1));
    assert_eq!(record.parent_version, Some(SdrVersion(0)));
    assert_eq!(record.transition_id, id("tr-1"));
    assert_eq!(
        record.actor,
        parse(json!({ "kind": "agent", "agent_id": "test_agent" }))
    );
    assert_eq!(record.reason.as_str(), "Change the design.");
    assert_eq!(record.evidence_ids, vec![id("ev-gate")]);
    assert_eq!(record.harness, harness());
    assert_eq!(
        record.committed_at,
        Timestamp(f.clock.now()),
        "committed_at"
    );

    let stored = f
        .repository
        .document(&f.project, &SdrVersion(1))
        .await
        .expect("stored document");
    assert_eq!(stored, expected);
    let digest = sha2::Sha256::digest(serde_json::to_vec(&stored).expect("serialize"));
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(record.document_sha256.as_str(), format!("sha256:{hex}"));
    assert_eq!(
        f.repository
            .version_record(&f.project, &SdrVersion(1))
            .await
            .expect("stored record"),
        record
    );
}

#[tokio::test]
async fn version_zero_is_the_empty_design() {
    let f = fixture().await;
    let doc = f
        .repository
        .document(&f.project, &SdrVersion(0))
        .await
        .expect("version 0");
    assert!(doc.requirements.is_empty() && doc.design.parameters.is_empty());
    assert_eq!(doc.parent_version, None);
    let again = f
        .manager
        .create_project(&f.project, initial(), &f.ctx)
        .await;
    assert_eq!(again.map(|_| ()).unwrap_err().code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn reading_a_slice_keeps_the_listed_kinds_and_ids() {
    let f = fixture().await;
    let ops = vec![
        add_requirement("req-1"),
        add_requirement("req-2"),
        set_parameter("par-1", 1.0),
        add_assumption("asm-1"),
    ];
    let validated = valid(f.validate(transition("tr-1", 0, ops)).await);
    f.manager
        .commit(validated, commit_info(&[]), &f.ctx)
        .await
        .expect("commit");

    let slice = SdrSlice {
        kinds: vec![ElementKind::Parameter],
        ids: vec![id("req-2")],
    };
    let doc = f
        .manager
        .read(&f.project, &slice, &SdrVersion(1), &f.ctx)
        .await
        .expect("read");
    let ids: Vec<&str> = doc
        .requirements
        .iter()
        .map(|r| r.id.as_str())
        .chain(doc.design.parameters.iter().map(|p| p.id.as_str()))
        .chain(doc.design.assumptions.iter().map(|a| a.id.as_str()))
        .collect();
    assert_eq!(ids, ["req-2", "par-1"]);
    let missing = f
        .manager
        .read(&f.project, &slice, &SdrVersion(7), &f.ctx)
        .await;
    assert_eq!(missing.map(|_| ()).unwrap_err().code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn rejecting_consumes_the_transition() {
    let f = fixture().await;
    let validated = valid(
        f.validate(transition("tr-1", 0, vec![add_requirement("r")]))
            .await,
    );
    let rejection = f
        .manager
        .reject(validated, vec!["the commit gate refused".to_owned()]);
    assert_eq!(rejection.transition_id, id("tr-1"));
    assert_eq!(rejection.base_version, SdrVersion(0));
    assert_eq!(
        f.manager.head(&f.project, &f.ctx).await.expect("head"),
        Some(SdrVersion(0))
    );
}

#[tokio::test]
async fn a_cancelled_context_stops_validation() {
    let f = fixture().await;
    let ctx = contracts::ExecutionContext::new(id("corr-cancel"));
    ctx.cancel();
    let proposed = f.manager.propose(
        f.project.clone(),
        transition("tr-1", 0, vec![add_requirement("r")]),
    );
    let result = f.manager.validate(proposed, &ctx).await;
    assert_eq!(result.map(|_| ()).unwrap_err().code(), ErrorCode::Cancelled);
}

#[tokio::test]
async fn documents_are_upgraded_and_checked() {
    let f = fixture().await;
    let example: serde_json::Value =
        serde_json::from_str(include_str!("../../../schemas/sdr/examples/system.json"))
            .expect("example JSON");
    let upgraded = f
        .manager
        .upgrade_document(example.clone())
        .expect("the v0 example is current");
    assert_eq!(serde_json::to_value(&upgraded).expect("serialize"), example);

    let mut unknown = example.clone();
    unknown["schema_version"] = json!("9.9");
    let mut broken = example;
    broken["requirements"] = json!([{ "id": "r" }]);
    for doc in [unknown, broken] {
        assert_eq!(
            f.manager.upgrade_document(doc).unwrap_err().code(),
            ErrorCode::InvalidInput
        );
    }
}
