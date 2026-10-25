//! The gateway pipeline with testkit's fake tools: every branch of blueprint
//! section 14.2 and the replay rules of ADR-0026, without a tool server.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use contracts::types::{ApprovalAnswer, EventPayload, EvidenceBody, StepOutcome};
use contracts::{ErrorCode, EvidenceStore, StepStore, Tool, ToolOutput};
use serde_json::{Value, json};
use support::{
    Harness, Listed, agent, autonomy, call, measure, measure_result, overlay, plate, plate_result,
};
use testkit::builders::{id, tool_request};
use testkit::fakes::FakeTool;
use tools::{ServerTools, ToolObservation, ToolRegistry};

fn returning(name: &str, result: Value) -> Arc<FakeTool> {
    Arc::new(FakeTool::new(name, move |_| {
        Ok(ToolOutput {
            structured: result.clone(),
        })
    }))
}

fn server(tools: &[Arc<FakeTool>]) -> Vec<ServerTools> {
    vec![ServerTools {
        server_id: id("build123d"),
        tools: tools.iter().map(|t| t.clone() as Arc<dyn Tool>).collect(),
    }]
}

fn plate_tool() -> Arc<FakeTool> {
    returning("create_plate", plate_result())
}

fn harness(listed: &[Listed], tools: &[Arc<FakeTool>]) -> Harness {
    Harness::new(&[overlay(listed)], &autonomy(None), server(tools))
}

fn error_code(observation: &ToolObservation) -> &str {
    match observation {
        ToolObservation::Failed { error, .. } => error.code.as_str(),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_valid_call_returns_the_result_with_evidence_and_events() {
    let tool = returning("measure", measure_result());
    let h = harness(&[Listed::tool("measure")], std::slice::from_ref(&tool));
    let part = plate_result()["part"].clone();
    let request = tool_request("measure", "step-1", measure(&part));
    let agent = agent();

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    let ToolObservation::Succeeded {
        tool_version,
        result,
        evidence_ids,
        replayed,
    } = observed
    else {
        panic!("expected Succeeded, got {observed:?}");
    };
    assert!(!replayed);
    assert_eq!(tool_version.as_str(), "0.1.0");
    assert_eq!(Value::Object(result), measure_result());
    assert_eq!(tool.calls(), 1);

    let [evidence_id] = evidence_ids.as_slice() else {
        panic!("expected one evidence id, got {evidence_ids:?}");
    };
    let evidence = h.evidence.get(evidence_id).await.unwrap();
    assert_eq!(*evidence.sdr_version, 3);
    let EvidenceBody::ToolObservation {
        tool,
        outputs,
        step_id,
        ..
    } = &evidence.body
    else {
        panic!("expected a tool observation, got {:?}", evidence.body);
    };
    assert_eq!(tool.as_str(), "measure");
    assert_eq!(step_id.as_str(), "step-1");
    let names: Vec<&str> = outputs.iter().map(|q| q.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "bounding_box.height",
            "bounding_box.length",
            "bounding_box.width",
            "mass",
            "volume"
        ]
    );

    assert_eq!(
        h.event_types().await,
        [
            "gate_decided",
            "gate_decided",
            "tool_call_started",
            "tool_call_completed"
        ]
    );
    let record = h.steps.get(&request.idempotency_key).await.unwrap();
    assert!(matches!(
        record.outcome,
        Some(StepOutcome::Succeeded { .. })
    ));
}

#[tokio::test]
async fn a_replayed_step_returns_the_stored_result_without_calling_the_tool() {
    let tool = plate_tool();
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let request = tool_request("create_plate", "step-1", plate());
    let agent = agent();
    let first = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    let events_after_first = h.events().await.len();

    let again = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(tool.calls(), 1, "the tool was called again");
    match (&first, &again) {
        (
            ToolObservation::Succeeded {
                result: a,
                evidence_ids: ea,
                replayed: false,
                ..
            },
            ToolObservation::Succeeded {
                result: b,
                evidence_ids: eb,
                replayed: true,
                ..
            },
        ) => {
            assert_eq!(a, b);
            assert_eq!(ea, eb);
        }
        other => panic!("expected a first and a replayed success, got {other:?}"),
    }
    assert_eq!(
        h.events().await.len(),
        events_after_first,
        "a replay appended events"
    );
}

#[tokio::test]
async fn a_key_reused_for_other_arguments_is_a_conflict() {
    let tool = plate_tool();
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());
    h.gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    let mut other = plate();
    other["length_mm"] = json!(120);
    let reused = tool_request("create_plate", "step-1", other);

    let err = h
        .gateway
        .call(call(&reused, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::Conflict);
    assert_eq!(tool.calls(), 1);
}

#[tokio::test]
async fn invalid_arguments_never_reach_the_tool() {
    let tool = plate_tool();
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let agent = agent();
    let mut bad = plate();
    bad["thickness_mm"] = json!(0);
    bad["script"] = json!("import os");
    let request = tool_request("create_plate", "step-1", bad);

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(error_code(&observed), "invalid_input");
    assert_eq!(tool.calls(), 0);
}

#[tokio::test]
async fn a_result_that_breaks_its_schema_is_a_failure_not_a_success() {
    let tool = returning(
        "create_plate",
        json!({ "part": { "part_id": "not-a-hash" } }),
    );
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(error_code(&observed), "tool_failed");
    assert!(
        h.evidence
            .list_for_run(&support::run_id())
            .await
            .unwrap()
            .is_empty()
    );
    let replayed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    assert!(matches!(
        replayed,
        ToolObservation::Failed { replayed: true, .. }
    ));
    assert_eq!(tool.calls(), 1);
}

#[tokio::test]
async fn a_tool_error_is_returned_as_a_failure() {
    let tool = Arc::new(FakeTool::failing(
        "create_plate",
        "invalid_geometry: holes meet",
    ));
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(error_code(&observed), "tool_failed");
    let events = h.events().await;
    let completed = events.last().unwrap();
    assert!(matches!(
        &completed.payload,
        EventPayload::ToolCallCompleted { error: Some(e), .. } if e.code.as_str() == "tool_failed"
    ));
}

#[tokio::test]
async fn a_slow_tool_times_out_as_a_failure() {
    let tool = Arc::new(
        FakeTool::new("create_plate", |_| {
            Ok(ToolOutput {
                structured: plate_result(),
            })
        })
        .with_delay(Duration::from_secs(3)),
    );
    let h = harness(
        &[Listed::tool("create_plate").timeout(1)],
        std::slice::from_ref(&tool),
    );
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let started = std::time::Instant::now();
    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(error_code(&observed), "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the timeout did not cut the call short"
    );
    let record = h.steps.get(&request.idempotency_key).await.unwrap();
    assert!(matches!(record.outcome, Some(StepOutcome::Failed { .. })));
}

#[tokio::test]
async fn cancelling_the_step_stops_the_call() {
    let tool = Arc::new(
        FakeTool::new("create_plate", |_| {
            Ok(ToolOutput {
                structured: plate_result(),
            })
        })
        .with_delay(Duration::from_secs(30)),
    );
    let h = harness(
        &[Listed::tool("create_plate").timeout(60)],
        std::slice::from_ref(&tool),
    );
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());
    let ctx = h.step_ctx("step-1");
    let canceller = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        canceller.cancel();
    });

    let observed = tokio::time::timeout(
        Duration::from_secs(5),
        h.gateway.call(call(&request, &agent), &ctx),
    )
    .await
    .expect("cancellation did not stop the call")
    .unwrap();

    assert_eq!(error_code(&observed), "cancelled");
}

#[tokio::test]
async fn a_tool_without_an_overlay_entry_is_denied_and_the_denial_recorded() {
    let plate_tool = plate_tool();
    let measure_tool = returning("measure", measure_result());
    let h = harness(
        &[Listed::tool("create_plate")],
        &[plate_tool, measure_tool.clone()],
    );
    assert_eq!(h.gateway.registry().available(), ["create_plate"]);
    let unavailable: Vec<&str> = h
        .gateway
        .registry()
        .unavailable()
        .iter()
        .map(|t| t.as_str())
        .collect();
    assert_eq!(unavailable, ["measure"]);
    let agent = agent();
    let part = plate_result()["part"].clone();
    let request = tool_request("measure", "step-1", measure(&part));

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    let ToolObservation::Denied { reasons } = observed else {
        panic!("expected Denied, got {observed:?}");
    };
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("not listed in config/tools/")),
        "{reasons:?}"
    );
    assert_eq!(measure_tool.calls(), 0);
    let denied = h.events().await.into_iter().any(|e| {
        matches!(
            e.payload,
            EventPayload::GateDecided {
                decision: contracts::types::GateDecision::Deny,
                ..
            }
        )
    });
    assert!(denied, "the deny decision is not in the event log");
}

#[tokio::test]
async fn an_asked_call_waits_for_approval_and_then_runs() {
    let tool = plate_tool();
    let h = Harness::new(
        &[overlay(&[Listed::tool("create_plate")])],
        &autonomy(Some("create_plate")),
        server(std::slice::from_ref(&tool)),
    );
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let asked = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    assert!(
        matches!(asked, ToolObservation::ApprovalRequired { .. }),
        "{asked:?}"
    );
    assert_eq!(tool.calls(), 0);

    let mut approved = call(&request, &agent);
    approved.approval = Some(ApprovalAnswer::Approve);
    let ran = h
        .gateway
        .call(approved, &h.step_ctx("step-1"))
        .await
        .unwrap();
    assert!(
        matches!(
            ran,
            ToolObservation::Succeeded {
                replayed: false,
                ..
            }
        ),
        "{ran:?}"
    );
    assert_eq!(tool.calls(), 1);
}

#[tokio::test]
async fn an_interrupted_idempotent_step_runs_again() {
    let tool = plate_tool();
    let h = harness(&[Listed::tool("create_plate")], std::slice::from_ref(&tool));
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());
    interrupt(&h, &request).await;

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert!(
        matches!(
            observed,
            ToolObservation::Succeeded {
                replayed: false,
                ..
            }
        ),
        "{observed:?}"
    );
    assert_eq!(tool.calls(), 1);
}

#[tokio::test]
async fn an_interrupted_step_of_a_tool_that_is_not_idempotent_is_not_repeated() {
    let tool = plate_tool();
    let h = harness(
        &[Listed::tool("create_plate").not_idempotent()],
        std::slice::from_ref(&tool),
    );
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());
    interrupt(&h, &request).await;

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(error_code(&observed), "conflict");
    assert_eq!(tool.calls(), 0);
    let again = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    assert!(
        matches!(again, ToolObservation::Failed { replayed: true, .. }),
        "{again:?}"
    );
    assert_eq!(tool.calls(), 0);
}

#[tokio::test]
async fn calls_to_a_tool_that_is_not_concurrency_safe_run_one_at_a_time() {
    let tool = Arc::new(
        FakeTool::new("create_plate", |_| {
            Ok(ToolOutput {
                structured: plate_result(),
            })
        })
        .with_delay(Duration::from_millis(300)),
    );
    let h = Arc::new(harness(
        &[Listed::tool("create_plate").exclusive().timeout(5)],
        std::slice::from_ref(&tool),
    ));
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..3 {
        let h = h.clone();
        handles.push(tokio::spawn(async move {
            let step = format!("step-{i}");
            let request = tool_request("create_plate", &step, plate());
            let agent = agent();
            h.gateway
                .call(call(&request, &agent), &h.step_ctx(&step))
                .await
                .unwrap()
        }));
    }
    for handle in handles {
        assert!(matches!(
            handle.await.unwrap(),
            ToolObservation::Succeeded { .. }
        ));
    }
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "calls overlapped: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_registry_fails_fast_on_inconsistent_overlays() {
    let code = |listed: Value, offered: &[Arc<FakeTool>]| -> ErrorCode {
        let overlay = testkit::builders::parse(listed);
        match ToolRegistry::new(&[overlay], server(offered)) {
            Ok(_) => panic!("the registry accepted an inconsistent overlay"),
            Err(e) => e.code(),
        }
    };
    let entry = |name: &str, schemas: bool| {
        let mut tool = json!({
            "name": name, "access": "read_only", "external_side_effects": false,
            "idempotent": true, "concurrency_safe": true, "timeout_seconds": 5,
            "requires_approval": false
        });
        if schemas {
            tool["request_schema"] = json!("tools/build123d/create_plate_request.schema.json");
            tool["result_schema"] = json!("tools/build123d/create_plate_result.schema.json");
        }
        tool
    };
    let overlay = |tools: Vec<Value>, server: &str| json!({ "schema_version": "0.1", "server_id": server, "description": "d", "tools": tools });

    assert_eq!(
        code(overlay(vec![entry("create_plate", true)], "build123d"), &[]),
        ErrorCode::InvalidInput,
        "a listed tool the server does not offer"
    );
    assert_eq!(
        code(
            overlay(vec![entry("create_plate", false)], "build123d"),
            &[plate_tool()]
        ),
        ErrorCode::InvalidInput,
        "a listed tool without schemas"
    );
    assert_eq!(
        code(
            overlay(vec![entry("create_plate", true)], "other"),
            &[plate_tool()]
        ),
        ErrorCode::InvalidInput,
        "an overlay for a server that is not connected"
    );
    let twice = vec![plate_tool(), plate_tool()];
    assert_eq!(
        code(
            overlay(vec![entry("create_plate", true)], "build123d"),
            &twice
        ),
        ErrorCode::InvalidInput,
        "a tool offered twice"
    );
}

/// Leaves `request`'s step started without an outcome, as a worker killed
/// mid-call would. Begun under the harness's fence, as the worker had it.
async fn interrupt(h: &Harness, request: &contracts::ToolRequest) {
    let ctx = h.step_ctx(request.step_id.as_str());
    let started = h
        .steps
        .begin(
            &contracts::NewStep {
                idempotency_key: request.idempotency_key.clone(),
                run_id: support::run_id(),
                step_id: request.step_id.clone(),
                tool: request.tool.clone(),
                request_sha256: request_hash(request),
            },
            ctx.write_authority(),
        )
        .await
        .unwrap();
    assert!(matches!(started, contracts::StepStart::Started(_)));
}

/// The gateway's request hash, recomputed from the documented definition:
/// sha256 of the canonical JSON of `{"arguments": ..., "tool": ...}`.
fn request_hash(request: &contracts::ToolRequest) -> contracts::types::Sha256 {
    let canonical = json!({ "arguments": request.arguments, "tool": request.tool.as_str() });
    id(&testkit::builders::sha256_of(
        canonical.to_string().as_bytes(),
    ))
}
