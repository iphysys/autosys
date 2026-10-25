//! Gates on SDR commits and tool calls, through the GateRunner to the event
//! log and hooks. With evaluation's tests, this is PR 6's done-when: failing
//! evidence rejects the commit with that evidence attached, a commit the
//! autonomy policy flags returns `ask`, and every decision is in the event
//! log.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{AgentId, Identifier, RunId};
use contracts::types::{
    ApprovalAnswer, AutonomyPolicy, Event, EventPayload, Evidence, GateDecision, GatesPolicy,
    SdrDocument, SdrSlice, ToolOverlay, Transition,
};
use contracts::{
    Error, ErrorCode, EventLog, EventSubscriber, ExecutionContext, Gate, GateOutcome, GateRequest,
    GateSubject, GateSubjectKind,
};
use governance::{
    ApprovalGate, Decision, GateRunner, ObservedEventLog, SdrCommitGate, ToolPermissionGate,
    WriteScopeGate,
};
use serde_json::{Value, json};
use testkit::builders::{id, parse, sdr_document, tool_request};
use testkit::clock::ManualClock;
use testkit::fakes::RecordingSubscriber;
use testkit::leases::LeasedJob;
use testkit::stores::InMemoryEventLog;

fn gates_policy() -> GatesPolicy {
    parse(json!({
        "schema_version": "0.1",
        "sdr_commit": { "required_checks": ["schema", "units", "requirement.numeric_threshold"] }
    }))
}

fn autonomy_policy() -> AutonomyPolicy {
    parse(json!({
        "schema_version": "0.1",
        "approvals": [
            {
                "id": "approve_final_design",
                "description": "Ask a human before committing the design the run intends to stop on.",
                "trigger": { "kind": "sdr_commit", "final_only": true }
            },
            {
                "id": "approve_export",
                "description": "Ask before exporting.",
                "trigger": { "kind": "tool_call", "tool": "export_part" }
            }
        ]
    }))
}

fn overlay() -> ToolOverlay {
    let tool = |name: &str, requires_approval: bool| {
        json!({
            "name": name,
            "access": "mutating",
            "external_side_effects": false,
            "idempotent": true,
            "concurrency_safe": true,
            "timeout_seconds": 60,
            "requires_approval": requires_approval
        })
    };
    parse(json!({
        "schema_version": "0.1",
        "server_id": "cad",
        "description": "Test overlay.",
        "tools": [tool("create_part", false), tool("export_part", false), tool("delete_part", true)]
    }))
}

/// A hook that always fails: it must never stop the run.
struct FailingHook(Identifier);

#[async_trait]
impl EventSubscriber for FailingHook {
    fn id(&self) -> &Identifier {
        &self.0
    }

    async fn on_event(&self, _event: &Event, _ctx: &ExecutionContext) -> Result<(), Error> {
        Err(Error::new(ErrorCode::Internal, "hook bug"))
    }
}

struct Fixture {
    runner: GateRunner,
    log: Arc<InMemoryEventLog>,
    hook: Arc<RecordingSubscriber>,
    ctx: ExecutionContext,
}

fn fixture_with(extra: Vec<Arc<dyn Gate>>) -> Fixture {
    let clock = Arc::new(ManualClock::at_epoch());
    let leased = LeasedJob::new(clock.clone(), "run-1");
    let log = Arc::new(InMemoryEventLog::fenced_by(leased.leases()));
    let hook = Arc::new(RecordingSubscriber::new());
    let observed = ObservedEventLog::new(
        log.clone(),
        vec![hook.clone(), Arc::new(FailingHook(id("failing")))],
        Duration::from_secs(1),
    )
    .expect("observed log");
    let mut gates: Vec<Arc<dyn Gate>> = vec![
        Arc::new(ToolPermissionGate::new(&[overlay()]).expect("tool gate")),
        Arc::new(SdrCommitGate::new(&gates_policy()).expect("commit gate")),
        Arc::new(
            ApprovalGate::new(&autonomy_policy(), GateSubjectKind::SdrCommit).expect("approval"),
        ),
        Arc::new(
            ApprovalGate::new(&autonomy_policy(), GateSubjectKind::ToolCall).expect("approval"),
        ),
    ];
    gates.extend(extra);
    let runner = GateRunner::new(gates, Arc::new(observed), clock).expect("runner");
    Fixture {
        runner,
        log,
        hook,
        ctx: leased.ctx("corr-gates").with_run(id("run-1")),
    }
}

fn fixture() -> Fixture {
    fixture_with(Vec::new())
}

/// Evidence in the shape the evaluator writes.
fn check_evidence(evidence_id: &str, check: &str, verdict: &str, subject: &str) -> Evidence {
    parse(json!({
        "schema_version": "0.1",
        "id": evidence_id,
        "run_id": "run-1",
        "created_at": "2026-01-01T00:00:00Z",
        "sdr_version": 0,
        "subject_ids": if subject.is_empty() { json!([]) } else { json!([subject]) },
        "body": {
            "kind": "check",
            "check_id": check,
            "check_version": "0.1.0",
            "method": "deterministic",
            "inputs": [],
            "verdict": verdict,
            "message": format!("{check} says {verdict}")
        }
    }))
}

fn passing() -> Vec<Evidence> {
    vec![
        check_evidence("ev-schema", "schema", "pass", ""),
        check_evidence("ev-units", "units", "pass", "req-mass"),
        check_evidence(
            "ev-threshold",
            "requirement.numeric_threshold",
            "pass",
            "req-mass",
        ),
    ]
}

fn with_verdict(mut evidence: Vec<Evidence>, check: &str, verdict: &str) -> Vec<Evidence> {
    for record in &mut evidence {
        let mut json: Value = serde_json::to_value(&*record).expect("serialize");
        if json["body"]["check_id"] == check {
            json["body"]["verdict"] = json!(verdict);
            *record = parse(json);
        }
    }
    evidence
}

fn transition(transition_id: &str, is_final: bool) -> Transition {
    testkit::builders::transition(transition_id, 0, is_final)
}

impl Fixture {
    async fn commit_decision(
        &self,
        transition: &Transition,
        evidence: &[Evidence],
        approval: Option<ApprovalAnswer>,
    ) -> Decision {
        let base: SdrDocument = sdr_document("p1", 0, None);
        self.runner
            .decide(
                GateSubject::SdrCommit {
                    base: &base,
                    transition,
                    evidence,
                },
                approval,
                &self.ctx,
            )
            .await
            .expect("decide")
    }

    async fn tool_decision(&self, tool: &str, approval: Option<ApprovalAnswer>) -> Decision {
        let agent: AgentId = id("orchestrator");
        let request = tool_request(tool, "step-1", json!({}));
        self.runner
            .decide(
                GateSubject::ToolCall {
                    agent_id: &agent,
                    request: &request,
                },
                approval,
                &self.ctx,
            )
            .await
            .expect("decide")
    }

    async fn events(&self) -> Vec<Event> {
        self.log
            .read(&id::<RunId>("run-1"), 0, 1000)
            .await
            .expect("read events")
    }

    /// (gate, decision) of every gate_decided event, in order.
    async fn decisions(&self) -> Vec<(String, GateDecision)> {
        self.events()
            .await
            .into_iter()
            .filter_map(|e| match e.payload {
                EventPayload::GateDecided { gate, decision, .. } => {
                    Some((gate.as_str().to_owned(), decision))
                }
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn a_unit_mismatch_rejects_the_commit_with_its_evidence_attached() {
    let f = fixture();
    let evidence = with_verdict(passing(), "units", "fail");
    let decision = f
        .commit_decision(&transition("tr-1", false), &evidence, None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::RejectTransition);
    let attached: Vec<&str> = decision
        .outcome
        .evidence_ids
        .iter()
        .map(|e| e.as_str())
        .collect();
    assert!(attached.contains(&"ev-units"), "{attached:?}");
    assert!(
        decision
            .outcome
            .reasons
            .iter()
            .any(|r| r.contains("units failed")),
        "{:?}",
        decision.outcome.reasons
    );
    assert_eq!(decision.approval_id, None);
}

#[tokio::test]
async fn an_unmet_threshold_rejects_the_commit_with_its_evidence_attached() {
    let f = fixture();
    let evidence = with_verdict(passing(), "requirement.numeric_threshold", "fail");
    let decision = f
        .commit_decision(&transition("tr-1", false), &evidence, None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::RejectTransition);
    assert!(
        decision
            .outcome
            .evidence_ids
            .iter()
            .any(|e| e.as_str() == "ev-threshold")
    );
    // The commit gate's own event carries the evidence too.
    let events = f.events().await;
    let attached = events.iter().find_map(|e| match &e.payload {
        EventPayload::GateDecided {
            gate, evidence_ids, ..
        } if gate.as_str() == "sdr_commit" => Some(evidence_ids.clone()),
        _ => None,
    });
    assert_eq!(attached, Some(vec![id("ev-threshold")]));
}

#[tokio::test]
async fn passing_evidence_allows_the_commit_and_names_what_it_relied_on() {
    let f = fixture();
    let mut evidence = passing();
    evidence.push(check_evidence(
        "ev-text",
        "requirement.numeric_threshold",
        "unverified",
        "req-finish",
    ));
    let decision = f
        .commit_decision(&transition("tr-1", false), &evidence, None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::Allow);
    assert_eq!(decision.outcome.evidence_ids.len(), 4);
}

#[tokio::test]
async fn missing_evidence_denies_and_a_check_error_retries() {
    let f = fixture();
    let missing: Vec<Evidence> = passing().into_iter().skip(1).collect();
    let decision = f
        .commit_decision(&transition("tr-1", false), &missing, None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::Deny);
    assert!(decision.outcome.reasons[0].contains("schema produced no evidence"));

    let errored = with_verdict(passing(), "units", "error");
    let decision = f
        .commit_decision(&transition("tr-2", false), &errored, None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::Retry);
}

#[tokio::test]
async fn a_final_commit_asks_and_the_answer_decides() {
    let f = fixture();
    let final_transition = transition("tr-final", true);
    let asked = f.commit_decision(&final_transition, &passing(), None).await;
    assert_eq!(asked.outcome.decision, GateDecision::Ask);
    let approval_id = asked.approval_id.clone().expect("an approval id");
    let requested = f.events().await.into_iter().find_map(|e| match e.payload {
        EventPayload::ApprovalRequested {
            approval_id,
            gate,
            subject_id,
            ..
        } => Some((approval_id, gate, subject_id)),
        _ => None,
    });
    assert_eq!(
        requested,
        Some((
            approval_id.clone(),
            id("approval.sdr_commit"),
            id("tr-final")
        ))
    );

    let again = f.commit_decision(&final_transition, &passing(), None).await;
    assert_eq!(again.approval_id, Some(approval_id), "the same approval");

    let approved = f
        .commit_decision(&final_transition, &passing(), Some(ApprovalAnswer::Approve))
        .await;
    assert_eq!(approved.outcome.decision, GateDecision::Allow);
    let refused = f
        .commit_decision(&final_transition, &passing(), Some(ApprovalAnswer::Reject))
        .await;
    assert_eq!(refused.outcome.decision, GateDecision::RejectTransition);

    // An approval never overrides failing evidence.
    let failing = with_verdict(passing(), "units", "fail");
    let overridden = f
        .commit_decision(&final_transition, &failing, Some(ApprovalAnswer::Approve))
        .await;
    assert_eq!(overridden.outcome.decision, GateDecision::RejectTransition);
}

#[tokio::test]
async fn tool_calls_need_a_listed_tool_and_approval_where_configured() {
    let f = fixture();
    let cases = [
        ("create_part", None, GateDecision::Allow),
        ("format_disk", None, GateDecision::Deny),
        ("delete_part", None, GateDecision::Ask),
        (
            "delete_part",
            Some(ApprovalAnswer::Approve),
            GateDecision::Allow,
        ),
        (
            "delete_part",
            Some(ApprovalAnswer::Reject),
            GateDecision::Deny,
        ),
        ("export_part", None, GateDecision::Ask),
        (
            "export_part",
            Some(ApprovalAnswer::Approve),
            GateDecision::Allow,
        ),
    ];
    for (tool, approval, expected) in cases {
        let decision = f.tool_decision(tool, approval).await;
        assert_eq!(decision.outcome.decision, expected, "{tool} {approval:?}");
    }
}

#[tokio::test]
async fn every_decision_is_in_the_event_log_and_seen_by_hooks() {
    let f = fixture();
    f.commit_decision(&transition("tr-1", false), &passing(), None)
        .await;
    f.tool_decision("format_disk", None).await;
    assert_eq!(
        f.decisions().await,
        vec![
            ("sdr_commit".to_owned(), GateDecision::Allow),
            ("approval.sdr_commit".to_owned(), GateDecision::Allow),
            ("tool_permission".to_owned(), GateDecision::Deny),
            ("approval.tool_call".to_owned(), GateDecision::Allow),
        ]
    );
    // The failing hook did not stop anything, and the recording hook saw
    // exactly what the log holds.
    assert_eq!(f.hook.events(), f.events().await);
}

struct BrokenGate(Identifier);

#[async_trait]
impl Gate for BrokenGate {
    fn id(&self) -> &Identifier {
        &self.0
    }

    fn subject_kind(&self) -> GateSubjectKind {
        GateSubjectKind::SdrCommit
    }

    async fn decide(
        &self,
        _request: GateRequest<'_>,
        _ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error> {
        Err(Error::new(ErrorCode::Unavailable, "policy store is down"))
    }
}

#[tokio::test]
async fn a_failing_gate_denies_and_is_recorded() {
    let f = fixture_with(vec![Arc::new(BrokenGate(id("broken")))]);
    let decision = f
        .commit_decision(&transition("tr-1", false), &passing(), None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::Deny);
    assert!(
        f.decisions()
            .await
            .contains(&("broken".to_owned(), GateDecision::Deny))
    );
}

#[tokio::test]
async fn decisions_need_a_run_and_unique_gates() {
    let f = fixture();
    let base = sdr_document("p1", 0, None);
    let t = transition("tr-1", false);
    let evidence = passing();
    let result = f
        .runner
        .decide(
            GateSubject::SdrCommit {
                base: &base,
                transition: &t,
                evidence: &evidence,
            },
            None,
            &ExecutionContext::new(id("corr")),
        )
        .await;
    assert_eq!(result.unwrap_err().code(), ErrorCode::InvalidInput);

    let twice: Vec<Arc<dyn Gate>> = vec![
        Arc::new(SdrCommitGate::new(&gates_policy()).expect("gate")),
        Arc::new(SdrCommitGate::new(&gates_policy()).expect("gate")),
    ];
    let duplicate = GateRunner::new(
        twice,
        Arc::new(InMemoryEventLog::new()),
        Arc::new(ManualClock::at_epoch()),
    );
    assert_eq!(
        duplicate.map(|_| ()).unwrap_err().code(),
        ErrorCode::InvalidInput
    );

    let duplicate_tool = ToolPermissionGate::new(&[overlay(), overlay()]);
    assert_eq!(
        duplicate_tool.map(|_| ()).unwrap_err().code(),
        ErrorCode::InvalidInput
    );
}

fn write_scope(kinds: &[&str], ids: &[&str]) -> SdrSlice {
    parse(json!({ "kinds": kinds, "ids": ids }))
}

/// A transition against [`sdr_document`]'s version 0, which holds the
/// requirement `req-mass` and the parameter `part.mass`.
fn transition_with(operations: Value) -> Transition {
    parse(json!({
        "schema_version": "0.1",
        "id": "tr-scope",
        "base_version": 0,
        "actor": { "kind": "agent", "agent_id": "main_orchestrator" },
        "reason": "Change the design.",
        "final": false,
        "operations": operations
    }))
}

fn set_parameter(element_id: &str) -> Value {
    json!({
        "op": "set_parameter",
        "parameter": {
            "id": element_id,
            "value": { "value": 0.5, "unit": "kg" },
            "source": { "kind": "user" },
            "evidence_ids": []
        }
    })
}

fn add_requirement(element_id: &str) -> Value {
    json!({
        "op": "add_requirement",
        "requirement": {
            "id": element_id,
            "text": "A requirement the task did not ask for.",
            "priority": "must",
            "evidence_ids": []
        }
    })
}

fn link_evidence(element_id: &str) -> Value {
    json!({ "op": "link_evidence", "element_id": element_id, "evidence_id": "ev-1" })
}

async fn scope_decision(scope: &SdrSlice, transition: &Transition) -> GateOutcome {
    let gate = WriteScopeGate::new(scope).expect("gate");
    let base = sdr_document("p1", 0, None);
    gate.decide(
        GateRequest {
            subject: GateSubject::SdrCommit {
                base: &base,
                transition,
                evidence: &[],
            },
            approval: None,
        },
        &ExecutionContext::new(id("corr-scope")),
    )
    .await
    .expect("decide")
}

#[tokio::test]
async fn a_transition_inside_the_write_scope_is_allowed() {
    let scope = write_scope(&["parameter", "decision"], &[]);
    let transition = transition_with(json!([
        set_parameter("part.mass"),
        {
            "op": "add_decision",
            "decision": {
                "id": "dec-1",
                "choice": "A thinner plate.",
                "rationale": "Lighter.",
                "related_ids": ["req-mass"]
            }
        },
        link_evidence("part.mass")
    ]));
    let decided = scope_decision(&scope, &transition).await;
    assert_eq!(
        decided.decision,
        GateDecision::Allow,
        "{:?}",
        decided.reasons
    );
    assert!(decided.reasons.is_empty());
}

#[tokio::test]
async fn elements_outside_the_write_scope_are_denied_and_named() {
    let scope = write_scope(&["parameter"], &[]);
    let transition = transition_with(json!([
        set_parameter("part.mass"),
        add_requirement("req-new"),
        link_evidence("req-mass")
    ]));
    let decided = scope_decision(&scope, &transition).await;
    assert_eq!(decided.decision, GateDecision::Deny);
    assert_eq!(decided.reasons.len(), 2, "{:?}", decided.reasons);
    assert!(
        decided.reasons[0].starts_with("operation 1: requirement req-new is outside"),
        "{:?}",
        decided.reasons
    );
    assert!(
        decided.reasons[1].starts_with("operation 2: requirement req-mass is outside"),
        "{:?}",
        decided.reasons
    );
    assert!(
        decided.reasons[0].contains("kinds: parameter; ids: none"),
        "{:?}",
        decided.reasons
    );
    assert!(decided.evidence_ids.is_empty());
}

#[tokio::test]
async fn ids_in_the_scope_cover_elements_of_any_kind() {
    let scope = write_scope(&[], &["req-mass"]);
    let allowed =
        scope_decision(&scope, &transition_with(json!([link_evidence("req-mass")]))).await;
    assert_eq!(
        allowed.decision,
        GateDecision::Allow,
        "{:?}",
        allowed.reasons
    );

    let denied = scope_decision(
        &scope,
        &transition_with(json!([set_parameter("part.mass")])),
    )
    .await;
    assert_eq!(denied.decision, GateDecision::Deny);
}

#[tokio::test]
async fn an_evidence_link_takes_the_kind_of_what_it_links_to() {
    let scope = write_scope(&["parameter"], &[]);
    // Added by the same transition.
    let added = transition_with(json!([
        set_parameter("part.width"),
        link_evidence("part.width")
    ]));
    assert_eq!(
        scope_decision(&scope, &added).await.decision,
        GateDecision::Allow
    );
    // In no document: no kind covers it, so the gate fails closed.
    let unknown = scope_decision(&scope, &transition_with(json!([link_evidence("ghost")]))).await;
    assert_eq!(unknown.decision, GateDecision::Deny);
    assert!(
        unknown.reasons[0].contains("element ghost is outside"),
        "{:?}",
        unknown.reasons
    );
}

#[tokio::test]
async fn a_write_scope_denial_wins_and_is_recorded() {
    let scope = write_scope(&["decision"], &[]);
    let f = fixture_with(vec![Arc::new(WriteScopeGate::new(&scope).expect("gate"))]);
    let decision = f
        .commit_decision(&transition("tr-1", false), &passing(), None)
        .await;
    assert_eq!(decision.outcome.decision, GateDecision::Deny);
    assert!(
        decision.outcome.reasons[0]
            .starts_with("sdr_write_scope: operation 0: parameter part.mass"),
        "{:?}",
        decision.outcome.reasons
    );
    assert!(
        f.decisions()
            .await
            .contains(&("sdr_write_scope".to_owned(), GateDecision::Deny))
    );

    let gate = WriteScopeGate::new(&scope).expect("gate");
    let agent: AgentId = id("orchestrator");
    let request = tool_request("create_part", "step-1", json!({}));
    let wrong = gate
        .decide(
            GateRequest {
                subject: GateSubject::ToolCall {
                    agent_id: &agent,
                    request: &request,
                },
                approval: None,
            },
            &f.ctx,
        )
        .await;
    assert_eq!(wrong.unwrap_err().code(), ErrorCode::InvalidInput);
}
