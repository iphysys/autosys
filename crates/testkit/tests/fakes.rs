//! Behavior of the fakes that later PRs rely on, beyond their contracts.

use std::time::Duration;

use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{ApprovalAnswer, GateDecision};
use contracts::{
    ErrorCode, ExecutionContext, Gate, GateRequest, GateSubject, GateSubjectKind, ModelProvider,
    SdrRepository, Tool,
};
use serde_json::json;
use testkit::builders::{id, model_request, sdr_document, tool_request, transition};
use testkit::fakes::{
    FakeGate, FakeTool, Fault, FaultySdrRepository, RecordedModelProvider, ScriptedModelProvider,
    text_response,
};

fn ctx() -> ExecutionContext {
    ExecutionContext::new(id("corr-fakes"))
}

#[tokio::test]
async fn approval_gate_asks_then_follows_the_answer() {
    let gate = FakeGate::approval("approval", GateSubjectKind::SdrCommit);
    let base = sdr_document("p1", 0, None);
    let proposal = transition("tr-1", 0, true);
    let decide = |approval| {
        let request = GateRequest {
            subject: GateSubject::SdrCommit {
                base: &base,
                transition: &proposal,
                evidence: &[],
            },
            approval,
        };
        let gate = &gate;
        async move { gate.decide(request, &ctx()).await.expect("decide").decision }
    };
    assert_eq!(decide(None).await, GateDecision::Ask);
    assert_eq!(
        decide(Some(ApprovalAnswer::Approve)).await,
        GateDecision::Allow
    );
    assert_eq!(
        decide(Some(ApprovalAnswer::Reject)).await,
        GateDecision::Deny
    );
}

#[tokio::test]
async fn gate_rejects_the_wrong_subject_kind() {
    let gate = FakeGate::fixed("approval", GateSubjectKind::SdrCommit, GateDecision::Allow);
    let agent = id("test_agent");
    let request = tool_request("create_part", "step-1", json!({}));
    let result = gate
        .decide(
            GateRequest {
                subject: GateSubject::ToolCall {
                    agent_id: &agent,
                    request: &request,
                },
                approval: None,
            },
            &ctx(),
        )
        .await;
    assert_eq!(result.map_err(|e| e.code()), Err(ErrorCode::InvalidInput));
}

#[tokio::test]
async fn scripted_provider_records_requests_and_fails_when_exhausted() {
    let provider = ScriptedModelProvider::new([Ok(text_response("Done."))]);
    let request = model_request("Finish.");

    let cancelled = ctx();
    cancelled.cancel();
    let result = provider.complete(&request, &cancelled).await;
    assert_eq!(result.map_err(|e| e.code()), Err(ErrorCode::Cancelled));
    assert_eq!(
        provider.remaining(),
        1,
        "a cancelled call consumed the script"
    );

    provider
        .complete(&request, &ctx())
        .await
        .expect("scripted response");
    let exhausted = provider.complete(&request, &ctx()).await;
    assert_eq!(exhausted.map_err(|e| e.code()), Err(ErrorCode::Internal));
    assert_eq!(provider.requests(), vec![request.clone(), request]);
}

#[tokio::test]
async fn recorded_provider_rejects_a_request_that_differs_from_the_recording() {
    let provider = RecordedModelProvider::from_file(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/recordings/model-tool-round-trip.json"),
    )
    .expect("recording loads");

    let drifted = provider
        .complete(&model_request("Design a different part."), &ctx())
        .await;
    let error = drifted.expect_err("a drifted request is rejected");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert!(error.message().contains("`messages`"), "{error}");
    assert_eq!(
        provider.remaining(),
        2,
        "a rejected call consumed the recording"
    );

    let not_a_recording = RecordedModelProvider::from_json("{\"exchanges\": []}");
    assert_eq!(
        not_a_recording.err().map(|e| e.code()),
        Some(ErrorCode::InvalidInput)
    );
}

#[tokio::test]
async fn failing_tool_reports_tool_failed() {
    let tool = FakeTool::failing("create_part", "invalid geometry");
    let request = tool_request("create_part", "step-1", json!({}));
    let result = tool.invoke(&request, &ctx()).await;
    assert_eq!(result.map_err(|e| e.code()), Err(ErrorCode::ToolFailed));
    assert_eq!(tool.calls(), 1);
}

#[tokio::test]
async fn slow_tool_stops_when_cancelled() {
    let tool = FakeTool::echo("create_part").with_delay(Duration::from_secs(60));
    let request = tool_request("create_part", "step-1", json!({}));
    let context = ctx();
    let cancel = context.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    let result = tokio::time::timeout(Duration::from_secs(5), tool.invoke(&request, &context))
        .await
        .expect("cancellation interrupted the delay");
    assert_eq!(result.map_err(|e| e.code()), Err(ErrorCode::Cancelled));
    assert_eq!(tool.calls(), 1, "the started call is counted");
}

#[tokio::test]
async fn faulty_sdr_storage_fails_every_call_with_its_code() {
    let project: ProjectId = id("p1");
    let version = SdrVersion(0);
    let storage = FaultySdrRepository::new(Fault::Fail(ErrorCode::Unavailable));
    let codes = [
        storage
            .head(&project)
            .await
            .map(|_| ())
            .map_err(|e| e.code()),
        storage
            .document(&project, &version)
            .await
            .map(|_| ())
            .map_err(|e| e.code()),
        storage
            .version_record(&project, &version)
            .await
            .map(|_| ())
            .map_err(|e| e.code()),
        storage
            .append_version(
                &sdr_document("p1", 0, None),
                &testkit::builders::version_record("p1", 0, None, chrono::Utc::now()),
            )
            .await
            .map_err(|e| e.code()),
    ];
    assert_eq!(codes, [Err(ErrorCode::Unavailable); 4]);
}

#[tokio::test]
async fn faulty_sdr_storage_after_head_answers_the_head_and_fails_the_rest() {
    let project: ProjectId = id("p1");
    let storage = FaultySdrRepository::after_head(3, Fault::Fail(ErrorCode::Internal));
    assert_eq!(
        storage.head(&project).await.expect("head"),
        Some(SdrVersion(3))
    );
    let document = storage.document(&project, &SdrVersion(3)).await;
    assert_eq!(document.map_err(|e| e.code()), Err(ErrorCode::Internal));
}

#[tokio::test]
async fn a_hanging_sdr_storage_never_finishes() {
    let project: ProjectId = id("p1");
    let storage = FaultySdrRepository::new(Fault::Hang);
    let finished = tokio::time::timeout(Duration::from_millis(50), storage.head(&project)).await;
    assert!(
        finished.is_err(),
        "head returned although the storage hangs"
    );
}
