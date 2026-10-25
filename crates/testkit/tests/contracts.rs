//! Every contract suite, run against testkit's in-memory implementations and
//! fakes. This is PR 3's done-when test.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use contracts::types::{ApprovalAnswer, GateDecision, Verdict};
use contracts::{CheckSubject, GateRequest, GateSubject, GateSubjectKind};
use serde_json::json;
use testkit::builders::{evidence, id, model_request, sdr_document, tool_request, transition};
use testkit::contract::{self, FencedStores};
use testkit::fakes::{
    FakeCheck, FakeGate, FakeTool, RecordedModelProvider, RecordingSubscriber,
    ScriptedModelProvider, text_response, tool_call_response,
};
use testkit::stores::{
    InMemoryArtifactStore, InMemoryEventLog, InMemoryEvidenceStore, InMemoryJobStore,
    InMemorySdrRepository, InMemoryStepStore,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_job_store() {
    contract::job_store(|clock| async move { InMemoryJobStore::new(clock) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_event_log() {
    contract::event_log(|clock| async move {
        let jobs = InMemoryJobStore::new(clock);
        (InMemoryEventLog::fenced_by(jobs.leases()), jobs)
    })
    .await;
}

#[tokio::test]
async fn in_memory_sdr_repository() {
    contract::sdr_repository(|clock| async move {
        let jobs = InMemoryJobStore::new(clock);
        (InMemorySdrRepository::fenced_by(jobs.leases()), jobs)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_step_store() {
    contract::step_store(|clock| async move {
        let jobs = InMemoryJobStore::new(clock);
        (InMemoryStepStore::fenced_by(jobs.leases()), jobs)
    })
    .await;
}

/// Every in-memory store fenced by one job store's leases.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_fencing() {
    contract::fencing(|clock| async move {
        let jobs = InMemoryJobStore::new(clock);
        let leases = jobs.leases();
        FencedStores {
            events: Arc::new(InMemoryEventLog::fenced_by(leases.clone())),
            steps: Arc::new(InMemoryStepStore::fenced_by(leases.clone())),
            sdr: Arc::new(InMemorySdrRepository::fenced_by(leases)),
            jobs: Arc::new(jobs),
        }
    })
    .await;
}

#[tokio::test]
async fn in_memory_evidence_store() {
    contract::evidence_store(|| async { InMemoryEvidenceStore::new() }).await;
}

#[tokio::test]
async fn in_memory_artifact_store() {
    contract::artifact_store(|| async { InMemoryArtifactStore::new() }).await;
}

#[tokio::test]
async fn fake_tools() {
    let request = tool_request("echo", "step-1", json!({ "length": 100 }));
    contract::tool(&FakeTool::echo("echo"), request).await;
}

#[tokio::test]
async fn scripted_model_provider() {
    contract::model_provider(
        || ScriptedModelProvider::new([Ok(text_response("Done."))]),
        model_request("Finish."),
    )
    .await;
    contract::model_provider(
        || {
            ScriptedModelProvider::new([Ok(tool_call_response(&[
                ("call-1", "create_part", json!({ "length": 100 })),
                ("call-2", "measure_part", json!({})),
            ]))])
        },
        model_request("Design the part."),
    )
    .await;
}

#[tokio::test]
async fn recorded_model_provider() {
    let recording = || {
        RecordedModelProvider::from_file(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/recordings/model-tool-round-trip.json"),
        )
        .expect("recording loads")
    };
    contract::model_provider(recording, model_request("Design the part.")).await;
    contract::model_provider_tool_round_trip(recording, model_request("Design the part.")).await;
}

#[tokio::test]
async fn fake_check() {
    let base = sdr_document("p1", 0, None);
    let proposal = transition("tr-1", 0, false);
    let check = FakeCheck::with_verdict("units", "part.mass", Verdict::Pass);
    contract::check(&check, CheckSubject::Document(&base)).await;
    contract::check(
        &check,
        CheckSubject::Transition {
            base: &base,
            transition: &proposal,
        },
    )
    .await;
}

#[tokio::test]
async fn fake_gates() {
    let agent = id("test_agent");
    let request = tool_request("create_part", "step-1", json!({}));
    let tool_gate = FakeGate::fixed(
        "tool_permission",
        GateSubjectKind::ToolCall,
        GateDecision::Allow,
    );
    contract::gate(
        &tool_gate,
        GateRequest {
            subject: GateSubject::ToolCall {
                agent_id: &agent,
                request: &request,
            },
            approval: None,
        },
    )
    .await;

    let base = sdr_document("p1", 0, None);
    let proposal = transition("tr-1", 0, true);
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let records = [evidence("ev-1", "run-1", at, "pass")];
    let approval_gate = FakeGate::approval("approval", GateSubjectKind::SdrCommit);
    for approval in [
        None,
        Some(ApprovalAnswer::Approve),
        Some(ApprovalAnswer::Reject),
    ] {
        contract::gate(
            &approval_gate,
            GateRequest {
                subject: GateSubject::SdrCommit {
                    base: &base,
                    transition: &proposal,
                    evidence: &records,
                },
                approval,
            },
        )
        .await;
    }
}

#[tokio::test]
async fn recording_subscriber() {
    let subscriber = RecordingSubscriber::new();
    contract::event_subscriber(&subscriber).await;
    assert_eq!(subscriber.events().len(), 15, "one event per payload type");
}
