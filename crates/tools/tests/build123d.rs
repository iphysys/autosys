//! PR 8's done-when, against the real build123d container through its
//! forwarder. `cargo xtask test-tools` starts the container and runs these
//! with `BUILD123D_URL` set; they are ignored by a plain `cargo test`.
//!
//! By hand:
//!
//! ```sh
//! docker compose -f deploy/compose/tools.yaml up -d --wait build123d-forward
//! BUILD123D_URL=http://127.0.0.1:18123/mcp cargo test -p synth-tools --test build123d -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use std::time::{Duration, Instant};

use contracts::types::{EventPayload, EvidenceBody, GateDecision};
use contracts::{EvidenceStore, ExecutionContext};
use serde_json::Value;
use support::relay::Relay;
use support::{Harness, Listed, agent, autonomy, call, measure, overlay, plate, run_id};
use testkit::builders::{id, tool_request};
use tools::{McpServer, McpSettings, ToolObservation};

const NEEDS: &str = "needs the build123d container; run by `cargo xtask test-tools`";

fn url() -> String {
    std::env::var("BUILD123D_URL")
        .expect("BUILD123D_URL must point at the build123d MCP endpoint (see the module docs)")
}

/// `host:port` of the forwarder, for the relay.
fn target() -> String {
    let url = url();
    let rest = url
        .strip_prefix("http://")
        .expect("BUILD123D_URL is http://");
    rest.split('/').next().unwrap().to_owned()
}

fn settings() -> McpSettings {
    McpSettings {
        connect_timeout: Duration::from_secs(15),
        call_timeout: Duration::from_secs(60),
    }
}

async fn harness(endpoint: &str, listed: &[Listed]) -> Harness {
    let ctx = ExecutionContext::new(id("corr-connect"));
    let server = McpServer::connect(id("build123d"), endpoint, &settings(), &ctx)
        .await
        .expect("connect to build123d");
    Harness::new(
        &[overlay(listed)],
        &autonomy(None),
        vec![server.into_server_tools()],
    )
}

fn both_tools(timeout: u64) -> Vec<Listed> {
    vec![
        Listed::tool("create_plate").timeout(timeout),
        Listed::tool("measure").timeout(timeout),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the build123d container; run by `cargo xtask test-tools`"]
async fn discovery_lists_both_tools_over_the_pinned_protocol() {
    let ctx = ExecutionContext::new(id("corr-connect"));
    let server = McpServer::connect(id("build123d"), &url(), &settings(), &ctx)
        .await
        .expect(NEEDS);
    let mut names: Vec<&str> = server
        .tools()
        .iter()
        .map(|t| contracts::Tool::descriptor(t.as_ref()).name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["create_plate", "measure"]);
    let create = server
        .tools()
        .iter()
        .find(|t| contracts::Tool::descriptor(t.as_ref()).name.as_str() == "create_plate")
        .unwrap();
    let request = tool_request("create_plate", "step-contract", plate());
    testkit::contract::tool(create.as_ref(), request).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the build123d container; run by `cargo xtask test-tools`"]
async fn create_plate_then_measure_returns_normalized_results_with_evidence() {
    let h = harness(&url(), &both_tools(60)).await;
    let agent = agent();

    let create = tool_request("create_plate", "step-1", plate());
    let created = h
        .gateway
        .call(call(&create, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    let ToolObservation::Succeeded { result, .. } = created else {
        panic!("create_plate: expected Succeeded, got {created:?}");
    };
    let part = result.get("part").cloned().expect("a part handle");

    let request = tool_request("measure", "step-2", measure(&part));
    let measured = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-2"))
        .await
        .unwrap();
    let ToolObservation::Succeeded {
        result,
        evidence_ids,
        tool_version,
        replayed: false,
    } = measured
    else {
        panic!("measure: expected Succeeded, got {measured:?}");
    };
    assert_eq!(tool_version.as_str(), "0.1.0");
    let mass = result["mass"]["value"].as_f64().unwrap();
    assert!((mass - 122.589_736_908).abs() < 1e-6, "mass {mass} g");
    assert_eq!(result["mass"]["unit"], Value::from("g"));

    let evidence = h.evidence.get(&evidence_ids[0]).await.unwrap();
    let EvidenceBody::ToolObservation { outputs, tool, .. } = &evidence.body else {
        panic!("expected tool evidence, got {:?}", evidence.body);
    };
    assert_eq!(tool.as_str(), "measure");
    let mass_output = outputs
        .iter()
        .find(|q| q.name.as_str() == "mass")
        .expect("mass in the evidence");
    assert_eq!(mass_output.quantity.unit.as_str(), "g");
    assert!((mass_output.quantity.value - mass).abs() < f64::EPSILON);
    assert_eq!(
        h.evidence.list_for_run(&run_id()).await.unwrap().len(),
        2,
        "one evidence record per call"
    );

    let types = h.event_types().await;
    assert_eq!(
        types
            .iter()
            .filter(|t| **t == "tool_call_completed")
            .count(),
        2,
        "{types:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the build123d container; run by `cargo xtask test-tools`"]
async fn a_tool_without_an_overlay_entry_is_denied() {
    let relay = Relay::start(target()).await;
    let h = harness(&relay.url(), &[Listed::tool("create_plate").timeout(60)]).await;
    let unavailable: Vec<&str> = h
        .gateway
        .registry()
        .unavailable()
        .iter()
        .map(|t| t.as_str())
        .collect();
    assert_eq!(unavailable, ["measure"], "discovered but not listed");
    let agent = agent();
    let part = serde_json::json!({
        "part_id": "sha256:c184e3b64442c589f4fee88c0f62ec6e182cf4b5f0eed59e90cb693259dbf706",
        "plate": plate()
    });
    let request = tool_request("measure", "step-1", measure(&part));
    let posts = relay.posts();

    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert!(
        matches!(observed, ToolObservation::Denied { .. }),
        "{observed:?}"
    );
    assert_eq!(relay.posts(), posts, "a denied call reached the server");
    let denied = h.events().await.into_iter().any(|e| {
        matches!(
            e.payload,
            EventPayload::GateDecided {
                decision: GateDecision::Deny,
                ..
            }
        )
    });
    assert!(denied, "the denial is not in the event log");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the build123d container; run by `cargo xtask test-tools`"]
async fn a_timeout_returns_an_error_observation() {
    let relay = Relay::start(target()).await;
    let h = harness(&relay.url(), &both_tools(1)).await;
    relay.delay_replies(Duration::from_secs(3));
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let started = Instant::now();
    let observed = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    let ToolObservation::Failed {
        error,
        replayed: false,
    } = &observed
    else {
        panic!("expected Failed, got {observed:?}");
    };
    assert_eq!(error.code.as_str(), "timeout", "{error:?}");
    assert!(error.retryable);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the build123d container; run by `cargo xtask test-tools`"]
async fn a_replayed_step_returns_the_stored_result_without_calling_the_server() {
    let relay = Relay::start(target()).await;
    let h = harness(&relay.url(), &both_tools(60)).await;
    let agent = agent();
    let request = tool_request("create_plate", "step-1", plate());

    let first = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();
    let posts = relay.posts();
    assert!(posts > 0, "the relay saw no request");
    let again = h
        .gateway
        .call(call(&request, &agent), &h.step_ctx("step-1"))
        .await
        .unwrap();

    assert_eq!(relay.posts(), posts, "the replay called the server");
    match (first, again) {
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
}
