//! The query loop's contract: what ADR-0029 requires of the context's
//! caller, how a turn opens and closes, how tool failures and denials come
//! back, and the stream itself.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use context::{ByteEstimate, TokenCounter};
use contracts::types::{EventPayload, StopReason, ToolResultContent};
use contracts::{ContentBlock, Role};
use serde_json::json;
use support::*;
use testkit::fakes::FakeTool;

/// create_plate, measure, then a final proposal of the measured mass: a run
/// that succeeds in three steps.
fn succeeding_script() -> Vec<ScriptStep> {
    vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        call_tools(vec![("c2", "measure", measure_args())]),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[(
                "c3".to_owned(),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, true),
            )])
        }),
    ]
}

fn job() -> contracts::types::Job {
    job_for(task_json(&["R-mass"], roomy_budget()))
}

// ------------------------------------------------------ ADR-0029 caller ---

#[tokio::test]
async fn every_model_call_has_one_context_built_from_the_agents_prompt() {
    let world = World::new(Setup::new(succeeding_script())).await;
    let ran = world.run(job()).await;
    assert_eq!(ran.outcome().stop_reason, StopReason::Success);
    let requests = world.model.requests();
    assert_eq!(requests.len(), 3);
    // The builder appends context_assembled; the loop never does.
    assert_eq!(ran.count("context_assembled"), requests.len());
    assert_eq!(ran.count("model_call_completed"), requests.len());
    let system = expected_system();
    for request in &requests {
        assert_eq!(request.system, system);
    }
    // Each context_assembled comes right before its model call.
    let kinds = ran.kinds();
    for (i, kind) in kinds.iter().enumerate() {
        if *kind == "model_call_completed" {
            assert_eq!(kinds[i - 1], "context_assembled", "{kinds:?}");
        }
    }
    // Only the runtime's own events carry its actor.
    for event in &ran.events {
        let runtime_owned = matches!(
            event.payload,
            EventPayload::RunStarted { .. }
                | EventPayload::ModelCallCompleted { .. }
                | EventPayload::SdrTransition { .. }
                | EventPayload::Error { .. }
                | EventPayload::RunStopped { .. }
        );
        let by_runtime = matches!(
            &event.actor,
            contracts::types::Actor::System { component } if component.as_str() == "runtime"
        );
        assert_eq!(runtime_owned, by_runtime, "{:?}", event.payload);
    }
}

#[tokio::test]
async fn tool_results_are_passed_raw_and_wrapped_once() {
    let world = World::new(Setup::new(succeeding_script())).await;
    world.run(job()).await;
    let requests = world.model.requests();
    let mut seen = 0;
    for request in &requests[1..] {
        for result in last_results(request) {
            seen += 1;
            assert_eq!(
                result.envelope.matches("<untrusted").count(),
                1,
                "{}",
                result.envelope
            );
            assert_eq!(result.envelope.matches("</untrusted>").count(), 1);
            assert!(
                result.envelope.starts_with("<untrusted source=\"tool:"),
                "{}",
                result.envelope
            );
            assert!(matches!(result.content, ToolResultContent::Ok { .. }));
        }
    }
    assert_eq!(seen, 2);
    // The model's own blocks pass through unchanged.
    let third = &requests[2];
    assert_eq!(
        roles(third),
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
    assert!(matches!(
        &third.messages[1].content[0],
        ContentBlock::ToolCall { id, .. } if id == "c1"
    ));
}

#[tokio::test]
async fn the_tool_definitions_are_counted_into_the_context_and_come_from_the_schemas() {
    let world = World::new(Setup::new(vec![
        reply("Stuck."),
        reply("Stuck."),
        reply("Stuck."),
        reply("Stuck."),
    ]))
    .await;
    let ran = world.run(job()).await;
    let request = &world.model.requests()[0];
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["sdr_read", "sdr_propose", "create_plate", "measure"]
    );
    assert!(
        request.tools[1]
            .description
            .starts_with("Proposes one change")
    );
    assert!(
        request.tools[2]
            .description
            .starts_with("Arguments of the build123d create_plate tool")
    );
    // The definitions are self-contained.
    assert!(request.tools[1].input_schema["$defs"]["Operation"].is_object());

    let profile = context_profiles().resolve(None).expect("default").clone();
    let definitions = serde_json::to_string(&request.tools).expect("serialize");
    let reserved = ByteEstimate::for_profile(&profile).count(&definitions);
    let total = ran
        .events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ContextAssembled { total_tokens, .. } => Some(*total_tokens),
            _ => None,
        })
        .expect("a context");
    assert!(
        total > reserved,
        "the context total {total} includes the definitions {reserved}"
    );
    let system_and_messages = ByteEstimate::for_profile(&profile).count(&request.system);
    assert!(total >= reserved + system_and_messages);
}

// ----------------------------------------------------------------- turns ---

#[tokio::test]
async fn a_commit_closes_the_turn_and_the_next_context_is_rebuilt() {
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "measure", measure_args())]),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[
                (
                    "c2".to_owned(),
                    "sdr_propose".to_owned(),
                    propose_mass(MASS_G, &evidence, false),
                ),
                // Not run: the commit before it closes the turn.
                ("c3".to_owned(), "create_plate".to_owned(), plate()),
            ])
        }),
        reply("Stuck."),
        reply("Stuck."),
        reply("Stuck."),
        reply("Stuck."),
    ]))
    .await;
    let ran = world.run(job()).await;
    assert_eq!(world.head().await, 2);
    assert_eq!(
        world.create_plate.calls(),
        0,
        "the call after the commit did not run"
    );
    let requests = world.model.requests();
    assert_eq!(requests[1].messages.len(), 3, "the turn was open");
    assert_eq!(requests[2].messages.len(), 1, "the commit closed the turn");
    let rebuilt = context_text(&requests[2]);
    assert!(
        rebuilt.contains("version=\"2\""),
        "the design is read at the new version"
    );
    assert!(rebuilt.contains("\"id\":\"part.mass\""), "{rebuilt}");
    // A turn that ends with a reply also closes.
    assert_eq!(requests[3].messages.len(), 1);
    assert_eq!(ran.outcome().stop_reason, StopReason::Failure);
}

#[tokio::test]
async fn a_tool_failure_comes_back_as_an_error_result_and_the_run_continues() {
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![("c1", "create_plate", plate())]),
            call_tools(vec![("c2", "measure", measure_args())]),
            from_request(|request| {
                let evidence = last_evidence(request);
                tool_use(&[(
                    "c3".to_owned(),
                    "sdr_propose".to_owned(),
                    propose_mass(MASS_G, &evidence, true),
                )])
            }),
        ])
        .create_plate(FakeTool::failing(
            "create_plate",
            "OpenCASCADE could not build the solid",
        )),
    )
    .await;
    let ran = world.run(job()).await;
    let second = &world.model.requests()[1];
    let [result] = last_results(second).try_into().expect("one result");
    assert!(result.is_error);
    let ToolResultContent::Error { error } = &result.content else {
        panic!("expected an error result, got {:?}", result.content);
    };
    assert_eq!(error.code.as_str(), "tool_failed");
    assert!(error.message.as_str().contains("OpenCASCADE"));
    // The failure was one repair; the run went on and succeeded.
    assert_eq!(ran.outcome().stop_reason, StopReason::Success);
}

#[tokio::test]
async fn a_tool_the_task_does_not_list_is_denied_without_running() {
    let mut task = task_json(&["R-mass"], roomy_budget());
    task["tools"] = json!(["sdr_read", "sdr_propose", "measure"]);
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        reply("Stuck."),
        reply("Stuck."),
        reply("Stuck."),
    ]))
    .await;
    let ran = world.run(job_for(task)).await;
    assert_eq!(world.create_plate.calls(), 0);
    assert!(
        ran.errors()
            .iter()
            .any(|(code, message)| code == "permission_denied"
                && message.contains("create_plate")
                && message.contains("allowed: sdr_read, sdr_propose, measure"))
    );
    let requests = world.model.requests();
    let second = &requests[1];
    let names: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["sdr_read", "sdr_propose", "measure"],
        "only the task's tools are shown"
    );
    let [result] = last_results(second).try_into().expect("one result");
    assert!(result.is_error);
    assert!(matches!(result.content, ToolResultContent::Denied { .. }));
    // No gate was asked: the router refused it first.
    assert_eq!(ran.count("gate_decided"), 0);
}

// ---------------------------------------------------------------- stream ---

#[tokio::test]
async fn the_stream_yields_the_logs_events_gap_free_and_in_order_then_stopped() {
    let world = World::new(Setup::new(succeeding_script()).autonomy(committed_autonomy())).await;
    let first = world.run(job()).await;
    let continuation = first.outcome().continuation.clone().expect("waiting");
    let resumed = world
        .resume(continuation, contracts::types::ApprovalAnswer::Approve)
        .await;
    let mut streamed = first.events.clone();
    streamed.extend(resumed.events.clone());
    assert_eq!(
        streamed,
        world.logged().await,
        "every logged event, once, in order"
    );
    let seqs: Vec<u64> = streamed.iter().map(|e| e.seq.get()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(
        first.outcome().job.progress.last_event_seq,
        first.events.last().map(|e| e.seq.get()).unwrap()
    );
    assert_eq!(
        resumed.outcome().job.progress.last_event_seq,
        seqs.len() as u64
    );
    // Events other components append are streamed too.
    for kind in [
        "context_assembled",
        "gate_decided",
        "check_completed",
        "tool_call_completed",
        "approval_requested",
    ] {
        assert!(
            first.kinds().contains(&kind) || resumed.kinds().contains(&kind),
            "{kind}"
        );
    }
}

// Paused time keeps the wall time in the results equal too.
#[tokio::test(start_paused = true)]
async fn ids_are_deterministic() {
    let run = || async {
        let world = World::new(Setup::new(succeeding_script())).await;
        let ran = world.run(job()).await;
        let ids: Vec<String> = ran
            .events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::ToolCallStarted {
                    step_id,
                    idempotency_key,
                    ..
                } => Some(format!("{}={}", step_id.as_str(), idempotency_key.as_str())),
                EventPayload::SdrTransition { transition_id, .. } => {
                    Some(transition_id.as_str().to_owned())
                }
                EventPayload::GateDecided { subject_id, .. } => {
                    Some(subject_id.as_str().to_owned())
                }
                _ => None,
            })
            .collect();
        (ids, ran.outcome().result.clone())
    };
    let (first, first_result) = run().await;
    let (second, second_result) = run().await;
    assert_eq!(first, second);
    assert_eq!(first_result, second_result);
    assert!(
        first.contains(&"step-1.1=job-1:step-1.1".to_owned()),
        "{first:?}"
    );
    assert!(
        first.contains(&"step-2.1=job-1:step-2.1".to_owned()),
        "{first:?}"
    );
    let transition = first_result.committed_transition_ids[0].as_str().to_owned();
    assert!(
        transition.starts_with("tr-") && transition.len() == 35,
        "{transition}"
    );
    assert!(first.contains(&transition));
}
