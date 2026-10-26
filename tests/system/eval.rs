//! Exit criterion 6 in CI (ADR-0035): the path of `cargo xtask eval`, with
//! a scripted model. The dev benchmark's request goes to api-server on
//! Postgres, `api_client::drive` drives the run to its end and answers its
//! approval, and `evaluation::outcomes` scores every expected outcome of
//! part_design_001 from the report, the log and the SDR versions. Each model
//! call of the run then replays exactly from the Postgres replay store.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use api_client::drive::{DriveSettings, drive};
use api_client::{Client, ClientConfig, IdempotencyKey, Token};
use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::replay::{fetch, rebuild};
use contracts::types::{
    AgentSpec, ApprovalAnswer, EventPayload, ScoreResult, StopReason, TokenUsage,
};
use contracts::{
    ContentBlock, Environment, Error, ExecutionContext, ModelProvider, ModelRequest, ModelResponse,
    ModelStopReason,
};
use evaluation::benchmarks::run_request;
use evaluation::outcomes::{RunRecord, all_passed, score};
use infra::benchmarks::BenchmarkLoader;
use infra::config::ConfigLoader;
use serde_json::{Value, json};
use testkit::builders::id;

use crate::api::{Server, TOKEN};
use crate::support::db::TestDb;
use crate::support::*;

/// Creates the plate, measures it, then proposes L, W and T from the
/// measured bounding box and the measured mass, each citing the
/// measurement, as the final design: what part_design_001 asks for.
struct BenchmarkModel {
    id: Identifier,
}

#[async_trait]
impl ModelProvider for BenchmarkModel {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        ctx.ensure_not_cancelled()?;
        Ok(benchmark_step(request))
    }
}

fn benchmark_step(request: &ModelRequest) -> ModelResponse {
    let mut tools: HashMap<&str, &str> = HashMap::new();
    let mut done: HashSet<&str> = HashSet::new();
    let mut measured: Option<Value> = None;
    let mut calls = 0;
    for block in request.messages.iter().flat_map(|m| &m.content) {
        match block {
            ContentBlock::ToolCall { id, name, .. } => {
                tools.insert(id, name.as_str());
                calls += 1;
            }
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error: false,
            } => {
                let (Some(tool), Some(result)) =
                    (tools.get(call_id.as_str()).copied(), envelope_json(content))
                else {
                    continue;
                };
                if result["status"] == "ok" {
                    done.insert(tool);
                    if tool == "measure" {
                        measured = Some(result);
                    }
                }
            }
            _ => {}
        }
    }
    let call_id = format!("c{}", calls + 1);
    let step = |tool: &str, arguments: Value| tool_use(&[(call_id.as_str(), tool, arguments)]);
    match (
        done.contains("create_plate"),
        &measured,
        done.contains("sdr_propose"),
    ) {
        (false, _, _) => step("create_plate", plate()),
        (true, None, _) => step("measure", measure_args()),
        (true, Some(measured), false) => step("sdr_propose", propose_design(measured)),
        (true, Some(_), true) => ModelResponse {
            model_id: "claude-haiku-5-5".to_owned(),
            content: vec![ContentBlock::Text {
                text: "The design is committed.".to_owned(),
            }],
            stop_reason: ModelStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 1000,
                output_tokens: 100,
            },
        },
    }
}

/// L, W, T and mass from `measured`, the `measure` tool's result envelope.
fn propose_design(measured: &Value) -> Value {
    let evidence = measured["evidence_ids"].clone();
    let source = json!({ "kind": "tool_result", "evidence_id": evidence[0] });
    let result = &measured["result"];
    let box_ = &result["bounding_box"];
    let operations: Vec<Value> = [
        ("L", &box_["length"]),
        ("W", &box_["width"]),
        ("T", &box_["height"]),
        ("mass", &result["mass"]),
    ]
    .iter()
    .map(|(id, quantity)| {
        json!({ "op": "set_parameter", "parameter": { "id": id, "value": quantity,
            "source": source, "evidence_ids": evidence } })
    })
    .collect();
    json!({
        "reason": "Record the measured plate as the final design.",
        "final": true,
        "operations": operations
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dev_benchmark_is_driven_and_scored_through_the_api() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let worker = build(
        "worker-eval",
        &db,
        &config,
        Parts::new(Arc::new(BenchmarkModel {
            id: id("benchmark"),
        })),
    )
    .await;
    let server = Server::start(&db, &config).await;
    let client = Client::new(
        &server.base,
        Token::new(TOKEN.to_owned()).expect("a token"),
        ClientConfig {
            request_timeout: WAIT,
            stream_idle_timeout: WAIT,
            ..ClientConfig::default()
        },
    )
    .expect("a client");

    let problem = BenchmarkLoader::new()
        .expect("the benchmark schema")
        .load(&repo_root().join("evals/benchmarks/dev/part_design_001/problem.yaml"))
        .expect("the dev problem");
    let agent: AgentSpec = ConfigLoader::new(Environment::Production)
        .expect("the config schemas")
        .load(&config.path().join("agents/main_orchestrator.yaml"))
        .expect("the agent spec");
    let request = run_request(&problem, &agent).expect("a request");

    let running = Background::start(Arc::clone(&worker));
    let driven = drive(
        &client,
        &request,
        &IdempotencyKey::new("eval-001-1").expect("a key"),
        &DriveSettings {
            answer: ApprovalAnswer::Approve,
            comment: None,
            deadline: WAIT,
            grace: Duration::from_secs(10),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("the drive failed: {e}"));
    running.stop().await;

    assert!(!driven.cancelled);
    assert_eq!(driven.report.stop_reason, StopReason::Success);
    assert_eq!(driven.answered.len(), 1, "the final commit asked once");
    let last = driven.report.final_sdr_version.0;
    let read: Vec<u64> = driven
        .versions
        .iter()
        .map(|v| v.document.version.0)
        .collect();
    assert_eq!(
        read,
        (0..=last).collect::<Vec<_>>(),
        "every version, in order"
    );

    let scores = score(
        &problem,
        &RunRecord {
            report: &driven.report,
            events: &driven.events,
            versions: &driven.versions,
        },
    )
    .expect("scores");
    for s in &scores {
        assert_eq!(s.result, ScoreResult::Pass, "{s:#?}");
    }
    assert!(all_passed(&scores));
    assert_eq!(scores.len(), problem.expected_outcomes.len());

    // Every model call of the run replays from Postgres to its logged hash.
    let run_id = driven.run.run_id.clone();
    let mut replayed = 0;
    for event in &driven.events {
        let EventPayload::ContextAssembled { context_hash, .. } = &event.payload else {
            continue;
        };
        let Ok(record) = worker.replay.call(&run_id, event.seq.get()).await else {
            // A context whose call failed or was never made has no record.
            continue;
        };
        let blobs = fetch(worker.replay.as_ref(), &record).await.expect("blobs");
        let (request, _) = rebuild(&record, &blobs).expect("rebuilds");
        let rehashed = context::context_hash(&request.system, &request.messages).expect("a hash");
        assert_eq!(&rehashed, context_hash, "context at seq {}", event.seq);
        replayed += 1;
    }
    let completed = driven
        .events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::ModelCallCompleted { .. }))
        .count();
    assert_eq!(replayed, completed, "one record per completed call");
    // Create, measure, propose; the approved final commit ends the run.
    assert_eq!(completed, 3);

    server.stop().await;
    db.drop().await;
}
