//! Checks on transitions validated by SdrManager, through the Evaluator to
//! stored evidence and `check_completed` events. With governance's tests,
//! this is PR 6's done-when: a unit mismatch or an unmet threshold produces
//! failing evidence, which the SDR commit gate turns into a rejection.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::{ProjectId, RunId, SdrVersion};
use contracts::types::{EventPayload, Evidence, EvidenceBody, Transition, Verdict};
use contracts::{
    Check, CheckDescriptor, CheckFinding, CheckSubject, Error, ErrorCode, EventLog, EvidenceStore,
    ExecutionContext,
};
use evaluation::Evaluator;
use evaluation::checks::{NumericThresholdCheck, SchemaCheck, UnitsCheck};
use sdr::{InitialVersion, SdrManager, ValidatedTransition, Validation};
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::clock::ManualClock;
use testkit::leases::LeasedJob;
use testkit::stores::{InMemoryEventLog, InMemoryEvidenceStore, fenced_sdr_storage};

struct Fixture {
    manager: SdrManager,
    evaluator: Evaluator,
    evidence: Arc<InMemoryEvidenceStore>,
    events: Arc<InMemoryEventLog>,
    project: ProjectId,
    ctx: ExecutionContext,
}

fn standard_checks() -> Vec<Arc<dyn Check>> {
    vec![
        Arc::new(SchemaCheck::new().expect("schema check")),
        Arc::new(UnitsCheck::new().expect("units check")),
        Arc::new(NumericThresholdCheck::new().expect("threshold check")),
    ]
}

async fn fixture_with(checks: Vec<Arc<dyn Check>>) -> Fixture {
    let clock = Arc::new(ManualClock::at_epoch());
    let leased = LeasedJob::new(clock.clone(), "run-1");
    let manager =
        SdrManager::new(fenced_sdr_storage(leased.leases()), clock.clone()).expect("manager");
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let events = Arc::new(InMemoryEventLog::fenced_by(leased.leases()));
    let evaluator =
        Evaluator::new(checks, evidence.clone(), events.clone(), clock).expect("evaluator");
    let project: ProjectId = id("p1");
    let ctx = leased.ctx("corr-eval").with_run(id("run-1"));
    manager
        .create_project(
            &project,
            InitialVersion {
                transition_id: id("tr-init"),
                actor: parse(json!({ "kind": "system", "component": "test" })),
                reason: parse(json!("Start.")),
                harness: harness(),
                requirements: Vec::new(),
            },
            &ctx,
        )
        .await
        .expect("create project");
    Fixture {
        manager,
        evaluator,
        evidence,
        events,
        project,
        ctx,
    }
}

async fn fixture() -> Fixture {
    fixture_with(standard_checks()).await
}

fn harness() -> contracts::types::HarnessVersions {
    parse(json!({
        "harness_build": "test", "model_ids": [], "prompts": [], "skills": [], "agent_specs": []
    }))
}

fn transition(transition_id: &str, base: u64, operations: Vec<Value>) -> Transition {
    parse(json!({
        "schema_version": "0.1",
        "id": transition_id,
        "base_version": base,
        "actor": { "kind": "agent", "agent_id": "test_agent" },
        "reason": "Change the design.",
        "final": false,
        "operations": operations
    }))
}

fn requirement(req: &str, parameter: &str, criterion: Value) -> Value {
    json!({
        "op": "add_requirement",
        "requirement": {
            "id": req,
            "text": "A numeric requirement.",
            "priority": "must",
            "numeric": { "parameter_id": parameter, "criterion": criterion },
            "evidence_ids": []
        }
    })
}

fn text_requirement(req: &str) -> Value {
    json!({
        "op": "add_requirement",
        "requirement": {
            "id": req, "text": "Looks good.", "priority": "should", "evidence_ids": []
        }
    })
}

fn parameter(id: &str, value: f64, unit: &str, source: Value) -> Value {
    json!({
        "op": "set_parameter",
        "parameter": {
            "id": id,
            "value": { "value": value, "unit": unit },
            "source": source,
            "evidence_ids": []
        }
    })
}

fn user() -> Value {
    json!({ "kind": "user" })
}

fn le(value: f64, unit: &str) -> Value {
    json!({ "comparator": "le", "limit": { "value": value, "unit": unit } })
}

impl Fixture {
    async fn validated(&self, operations: Vec<Value>) -> ValidatedTransition {
        let proposed = self
            .manager
            .propose(self.project.clone(), transition("tr-1", 0, operations));
        match self
            .manager
            .validate(proposed, &self.ctx)
            .await
            .expect("validate")
        {
            Validation::Valid(v) => v,
            Validation::Invalid(i) => panic!("transition invalid: {:?}", i.problems),
        }
    }

    async fn evaluate(&self, operations: Vec<Value>) -> Vec<Evidence> {
        let validated = self.validated(operations).await;
        self.evaluator
            .evaluate(&validated, &self.ctx)
            .await
            .expect("evaluate")
    }
}

/// The verdict and message of the evidence `check` produced about `subject`.
fn verdict_of(evidence: &[Evidence], check: &str, subject: &str) -> (Verdict, String) {
    evidence
        .iter()
        .find_map(|e| match &e.body {
            EvidenceBody::Check {
                check_id,
                verdict,
                message,
                ..
            } if check_id.as_str() == check
                && (subject.is_empty() || e.subject_ids.iter().any(|s| s.as_str() == subject)) =>
            {
                Some((
                    *verdict,
                    message
                        .as_ref()
                        .map(|m| m.as_str().to_owned())
                        .unwrap_or_default(),
                ))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {check} evidence about {subject:?}"))
}

#[tokio::test]
async fn a_unit_mismatch_fails_the_units_check() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![
            requirement("req-mass", "part.mass", le(1.0, "kg")),
            parameter("part.mass", 120.0, "mm", user()),
        ])
        .await;
    let (verdict, message) = verdict_of(&evidence, "units", "req-mass");
    assert_eq!(verdict, Verdict::Fail, "{message}");
    assert!(message.contains("unit mismatch"), "{message}");
    // The threshold check cannot compare mm with kg: unverified, never pass.
    let (verdict, _) = verdict_of(&evidence, "requirement.numeric_threshold", "req-mass");
    assert_eq!(verdict, Verdict::Unverified);
    assert_eq!(verdict_of(&evidence, "schema", "").0, Verdict::Pass);
}

#[tokio::test]
async fn an_unknown_unit_fails_the_units_check() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![parameter("part.mass", 1.0, "[lb_av]", user())])
        .await;
    let (verdict, message) = verdict_of(&evidence, "units", "part.mass");
    assert_eq!(verdict, Verdict::Fail);
    assert!(message.contains("not a supported UCUM unit"), "{message}");
}

#[tokio::test]
async fn an_unmet_threshold_fails_the_requirement_check() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![
            requirement("req-mass", "part.mass", le(1.0, "kg")),
            parameter("part.mass", 1.2, "kg", user()),
        ])
        .await;
    let (verdict, message) = verdict_of(&evidence, "requirement.numeric_threshold", "req-mass");
    assert_eq!(verdict, Verdict::Fail);
    assert_eq!(message, "req-mass: part.mass = 1.2 kg is not <= 1 kg");
    assert_eq!(verdict_of(&evidence, "units", "req-mass").0, Verdict::Pass);
}

#[tokio::test]
async fn thresholds_compare_after_unit_conversion() {
    let f = fixture().await;
    let criteria = [
        ("req-le", le(0.3, "kg"), 300.0, "g", Verdict::Pass),
        ("req-le-over", le(0.3, "kg"), 301.0, "g", Verdict::Fail),
        (
            "req-ge",
            json!({ "comparator": "ge", "limit": { "value": 2, "unit": "mm" } }),
            0.25,
            "cm",
            Verdict::Pass,
        ),
        (
            "req-eq",
            json!({
                "comparator": "eq",
                "target": { "value": 50, "unit": "mm" },
                "tolerance": { "value": 0.1, "unit": "mm" }
            }),
            5.005,
            "cm",
            Verdict::Pass,
        ),
        (
            "req-within",
            json!({
                "comparator": "within",
                "min": { "value": 1, "unit": "L" },
                "max": { "value": 2, "unit": "L" }
            }),
            2500.0,
            "cm3",
            Verdict::Fail,
        ),
    ];
    let mut operations = Vec::new();
    for (req, criterion, value, unit, _) in &criteria {
        let par = format!("{req}.value");
        operations.push(requirement(req, &par, criterion.clone()));
        operations.push(parameter(&par, *value, unit, user()));
    }
    let evidence = f.evaluate(operations).await;
    for (req, _, _, _, expected) in &criteria {
        let (verdict, message) = verdict_of(&evidence, "requirement.numeric_threshold", req);
        assert_eq!(verdict, *expected, "{req}: {message}");
    }
}

#[tokio::test]
async fn undecidable_requirements_are_unverified() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![
            text_requirement("req-finish"),
            requirement("req-mass", "part.mass", le(1.0, "kg")),
        ])
        .await;
    for req in ["req-finish", "req-mass"] {
        let (verdict, message) = verdict_of(&evidence, "requirement.numeric_threshold", req);
        assert_eq!(verdict, Verdict::Unverified, "{req}: {message}");
    }
}

#[tokio::test]
async fn evidence_records_inputs_normalized_values_and_sources() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![
            requirement("req-mass", "part.mass", le(1.0, "kg")),
            parameter(
                "part.mass",
                212.0,
                "g",
                json!({ "kind": "tool_result", "evidence_id": "ev-measure-1" }),
            ),
        ])
        .await;
    let record = evidence
        .iter()
        .find(|e| matches!(&e.body, EvidenceBody::Check { check_id, .. } if check_id.as_str() == "requirement.numeric_threshold"))
        .expect("threshold evidence");
    let EvidenceBody::Check {
        inputs, verdict, ..
    } = &record.body
    else {
        unreachable!()
    };
    assert_eq!(*verdict, Verdict::Pass);
    let value = &inputs[0];
    assert_eq!(value.name.as_str(), "value");
    assert_eq!(value.quantity.value, 212.0);
    let normalized = value.normalized.as_ref().expect("normalized value");
    assert!((normalized.value - 0.212).abs() < 1e-12);
    assert_eq!(normalized.unit.as_str(), "kg");
    assert_eq!(
        value.source_evidence_id.as_ref().map(|e| e.as_str()),
        Some("ev-measure-1")
    );
    assert_eq!(record.sdr_version, SdrVersion(0), "the base version");
    assert_eq!(record.run_id, id::<RunId>("run-1"));
}

#[tokio::test]
async fn every_finding_is_stored_and_reported_as_an_event() {
    let f = fixture().await;
    let evidence = f
        .evaluate(vec![
            requirement("req-mass", "part.mass", le(1.0, "kg")),
            parameter("part.mass", 1.2, "kg", user()),
        ])
        .await;
    let run: RunId = id("run-1");
    let stored = f.evidence.list_for_run(&run).await.expect("list");
    assert_eq!(stored.len(), evidence.len());
    let events = f.events.read(&run, 0, 100).await.expect("events");
    assert_eq!(events.len(), evidence.len());
    for (event, record) in events.iter().zip(&evidence) {
        let EventPayload::CheckCompleted {
            evidence_id,
            verdict,
            ..
        } = &event.payload
        else {
            panic!("unexpected event {:?}", event.payload)
        };
        assert_eq!(evidence_id, &record.id);
        let EvidenceBody::Check {
            verdict: recorded, ..
        } = &record.body
        else {
            unreachable!()
        };
        assert_eq!(verdict, recorded);
    }
}

struct BrokenCheck(CheckDescriptor);

#[async_trait]
impl Check for BrokenCheck {
    fn descriptor(&self) -> &CheckDescriptor {
        &self.0
    }

    async fn evaluate(
        &self,
        _subject: CheckSubject<'_>,
        _ctx: &ExecutionContext,
    ) -> Result<Vec<CheckFinding>, Error> {
        Err(Error::new(ErrorCode::Unavailable, "solver is down"))
    }
}

#[tokio::test]
async fn a_check_that_fails_to_run_yields_error_evidence() {
    let broken = BrokenCheck(CheckDescriptor {
        id: id("broken"),
        version: parse(json!("0.1.0")),
        method: contracts::types::CheckMethod::Deterministic,
    });
    let f = fixture_with(vec![Arc::new(broken)]).await;
    let evidence = f.evaluate(vec![text_requirement("req-1")]).await;
    let (verdict, message) = verdict_of(&evidence, "broken", "");
    assert_eq!(verdict, Verdict::Error);
    assert!(message.contains("solver is down"), "{message}");
}

#[tokio::test]
async fn evaluation_needs_a_run_and_unique_checks() {
    let f = fixture().await;
    let validated = f.validated(vec![text_requirement("req-1")]).await;
    let no_run = ExecutionContext::new(id("corr"));
    let result = f.evaluator.evaluate(&validated, &no_run).await;
    assert_eq!(
        result.map(|_| ()).unwrap_err().code(),
        ErrorCode::InvalidInput
    );

    let twice: Vec<Arc<dyn Check>> = vec![
        Arc::new(UnitsCheck::new().expect("units")),
        Arc::new(UnitsCheck::new().expect("units")),
    ];
    let clock = Arc::new(ManualClock::at_epoch());
    let duplicate = Evaluator::new(
        twice,
        Arc::new(InMemoryEvidenceStore::new()),
        Arc::new(InMemoryEventLog::new()),
        clock,
    );
    assert_eq!(
        duplicate.map(|_| ()).unwrap_err().code(),
        ErrorCode::InvalidInput
    );
}

#[tokio::test]
async fn document_checks_refuse_a_bare_transition() {
    let check = UnitsCheck::new().expect("units");
    let base: contracts::types::SdrDocument = testkit::builders::sdr_document("p1", 0, None);
    let t = transition("tr-1", 0, vec![text_requirement("r")]);
    let ctx = ExecutionContext::new(id("corr"));
    let result = check
        .evaluate(
            CheckSubject::Transition {
                base: &base,
                transition: &t,
            },
            &ctx,
        )
        .await;
    assert_eq!(result.unwrap_err().code(), ErrorCode::InvalidInput);
}
