//! The two design tools: `sdr_read` stays inside the task's read slice, and
//! `sdr_propose` commits only through validation, the checks and the gates,
//! with one `sdr_transition` event per decided proposal.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use contracts::types::{
    EventPayload, GateDecision, SdrProposeResult, SdrReadResult, StopReason, ToolResultContent,
    Verdict,
};
use serde_json::{Value, json};
use support::*;

fn job_with(change: impl FnOnce(&mut Value)) -> contracts::types::Job {
    let mut task = task_json(&["R-mass"], budget(200_000, 20, 20, 600, 3));
    change(&mut task);
    job_for(task)
}

/// Gives up three times after the scripted steps, so a run ends.
fn then_give_up(mut script: Vec<ScriptStep>) -> Vec<ScriptStep> {
    script.extend((0..4).map(|_| reply("Stuck.")));
    script
}

fn ok_result<T: serde::de::DeserializeOwned>(seen: &SeenResult) -> T {
    let ToolResultContent::Ok { result, .. } = &seen.content else {
        panic!("expected an ok result, got {:?}", seen.content);
    };
    serde_json::from_value(Value::Object(result.clone())).expect("the result's schema")
}

/// The results the request after `step` (counting from 0) received.
fn results_after(world: &World, step: usize) -> Vec<SeenResult> {
    last_results(&world.model.requests()[step + 1])
}

/// (gate, decision) of each `gate_decided`, in order.
fn gate_decisions(ran: &Ran) -> Vec<(String, GateDecision)> {
    ran.events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::GateDecided { gate, decision, .. } => {
                Some((gate.as_str().to_owned(), *decision))
            }
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------ read ---

#[tokio::test]
async fn reads_stay_inside_the_read_slice() {
    let world = World::new(
        Setup::new(then_give_up(vec![call_tools(vec![
            ("r1", "sdr_read", json!({})),
            (
                "r2",
                "sdr_read",
                json!({ "ids": ["part.length", "R-mass"] }),
            ),
            ("r3", "sdr_read", json!({ "version": 0 })),
            ("r4", "sdr_read", json!({ "version": 9 })),
        ])]))
        .requirements(vec![
            mass_requirement("R-mass", 150.0),
            json!({
                "op": "set_parameter",
                "parameter": {
                    "id": "part.length",
                    "value": { "value": 110, "unit": "mm" },
                    "source": { "kind": "user" },
                    "evidence_ids": []
                }
            }),
        ]),
    )
    .await;
    let ran = world
        .run(job_with(|task| {
            task["sdr"]["read_slice"] = json!({ "kinds": ["requirement"], "ids": [] });
        }))
        .await;
    let results = results_after(&world, 0);
    assert_eq!(results.len(), 4);

    let whole: SdrReadResult = ok_result(&results[0]);
    assert_eq!(whole.document.version.0, 1);
    assert_eq!(whole.document.requirements.len(), 1);
    assert!(
        whole.document.design.parameters.is_empty(),
        "outside the slice"
    );

    let picked: SdrReadResult = ok_result(&results[1]);
    assert_eq!(picked.document.requirements.len(), 1);
    assert!(
        picked.document.design.parameters.is_empty(),
        "asking does not widen the slice"
    );

    let earlier: SdrReadResult = ok_result(&results[2]);
    assert_eq!(earlier.document.version.0, 0);
    assert!(earlier.document.requirements.is_empty());

    assert!(results[3].is_error);
    let ToolResultContent::Error { error } = &results[3].content else {
        panic!("expected an error, got {:?}", results[3].content);
    };
    assert_eq!(error.code.as_str(), "not_found");
    assert!(error.message.as_str().contains("the latest is 1"));

    // The context's design state is the same slice.
    let context = context_text(&world.model.requests()[0]);
    assert!(context.contains("\"id\":\"R-mass\""));
    assert!(!context.contains("part.length"));
    // A read records nothing.
    assert!(ran.transitions().is_empty());
    assert_eq!(ran.count("tool_call_started"), 0);
    // The version that did not exist was one failure.
    assert_eq!(ran.outcome().stop_reason, StopReason::Failure);
}

// --------------------------------------------------------------- propose ---

#[tokio::test]
async fn a_valid_proposal_commits_through_the_checks_and_gates() {
    let world = World::new(Setup::new(then_give_up(vec![
        call_tools(vec![("c1", "measure", measure_args())]),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[(
                "c2".to_owned(),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, false),
            )])
        }),
    ])))
    .await;
    let ran = world.run(job_with(|_| {})).await;
    assert_eq!(world.head().await, 2);
    let after_call = ran
        .kinds()
        .into_iter()
        .skip_while(|k| *k != "tool_call_completed")
        .skip(3) // tool_call_completed, context_assembled, model_call_completed
        .take(8)
        .collect::<Vec<_>>();
    assert_eq!(
        after_call,
        [
            "check_completed",
            "check_completed",
            "check_completed",
            "check_completed",
            "gate_decided",
            "gate_decided",
            "gate_decided",
            "sdr_transition"
        ]
    );
    assert_eq!(
        gate_decisions(&ran),
        [
            ("tool_permission".to_owned(), GateDecision::Allow),
            ("approval.tool_call".to_owned(), GateDecision::Allow),
            ("sdr_write_scope".to_owned(), GateDecision::Allow),
            ("sdr_commit".to_owned(), GateDecision::Allow),
            ("approval.sdr_commit".to_owned(), GateDecision::Allow),
        ]
    );
    let transition = ran
        .events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::SdrTransition {
                base_version,
                new_version,
                outcome,
                ..
            } => Some((
                base_version.0,
                new_version.as_ref().map(|v| v.0),
                outcome.to_string(),
            )),
            _ => None,
        })
        .expect("a transition");
    assert_eq!(transition, (1, Some(2), "committed".to_owned()));
    // The committed design holds the value, with the measurement as source.
    let rebuilt = context_text(&world.model.requests()[2]);
    assert!(
        rebuilt.contains("\"source\":{\"kind\":\"tool_result\",\"evidence_id\":\"ev-tool-"),
        "{rebuilt}"
    );
    assert!(
        rebuilt.contains(&format!("\"value\":{MASS_G}")),
        "{rebuilt}"
    );
}

#[tokio::test]
async fn an_invalid_proposal_is_rejected_before_any_check_runs() {
    let world = World::new(Setup::new(then_give_up(vec![call_tools(vec![(
        "c1",
        "sdr_propose",
        json!({
            "reason": "Link evidence to an element that does not exist.",
            "final": false,
            "operations": [{ "op": "link_evidence", "element_id": "ghost", "evidence_id": "ev-1" }]
        }),
    )])])))
    .await;
    let ran = world.run(job_with(|_| {})).await;
    assert_eq!(world.head().await, 1);
    assert_eq!(ran.count("check_completed"), 0);
    assert_eq!(ran.count("gate_decided"), 0);
    let transitions = ran.transitions();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].1, "rejected");
    assert!(
        transitions[0]
            .3
            .iter()
            .any(|r| r.contains("UnknownElement") && r.contains("ghost")),
        "{transitions:?}"
    );
    let [seen] = results_after(&world, 0).try_into().expect("one result");
    assert!(seen.is_error);
    let result: SdrProposeResult = ok_result(&seen);
    assert_eq!(result.outcome.to_string(), "rejected");
    assert!(result.checks.is_empty());
    assert_eq!(result.base_version.0, 1);
    assert_eq!(result.new_version, None);
}

#[tokio::test]
async fn a_proposal_a_check_fails_is_rejected_with_the_failing_evidence() {
    let world = World::new(
        Setup::new(then_give_up(vec![
            call_tools(vec![("c1", "measure", measure_args())]),
            from_request(|request| {
                let evidence = last_evidence(request);
                tool_use(&[(
                    "c2".to_owned(),
                    "sdr_propose".to_owned(),
                    propose_mass(MASS_G, &evidence, true),
                )])
            }),
        ]))
        .requirements(vec![mass_requirement("R-mass", 100.0)]),
    )
    .await;
    let ran = world.run(job_with(|_| {})).await;
    assert_eq!(world.head().await, 1);
    assert!(
        gate_decisions(&ran).contains(&("sdr_commit".to_owned(), GateDecision::RejectTransition))
    );
    assert_eq!(ran.transitions()[0].1, "rejected");
    let [seen] = results_after(&world, 1).try_into().expect("one result");
    assert!(seen.is_error);
    let result: SdrProposeResult = ok_result(&seen);
    let failed: Vec<_> = result
        .checks
        .iter()
        .filter(|c| c.verdict == Verdict::Fail)
        .collect();
    assert_eq!(failed.len(), 1, "{:?}", result.checks);
    assert_eq!(failed[0].check_id.as_str(), "requirement.numeric_threshold");
    assert!(
        result
            .reasons
            .iter()
            .any(|r| r.as_str().contains("is not <= 100 g"))
    );
    let ToolResultContent::Ok { evidence_ids, .. } = &seen.content else {
        panic!("an ok result");
    };
    assert!(evidence_ids.contains(&failed[0].evidence_id));
}

#[tokio::test]
async fn a_proposal_outside_the_write_scope_is_denied() {
    let world = World::new(Setup::new(then_give_up(vec![call_tools(vec![(
        "c1",
        "sdr_propose",
        propose_mass(MASS_G, &[], false),
    )])])))
    .await;
    let ran = world
        .run(job_with(|task| {
            task["sdr"]["write_scope"] = json!({ "kinds": ["decision"], "ids": [] });
        }))
        .await;
    assert_eq!(world.head().await, 1);
    assert!(gate_decisions(&ran).contains(&("sdr_write_scope".to_owned(), GateDecision::Deny)));
    let transitions = ran.transitions();
    assert_eq!(transitions[0].1, "rejected");
    assert!(
        transitions[0]
            .3
            .iter()
            .any(|r| r.contains("parameter part.mass is outside the task's write scope")),
        "{transitions:?}"
    );
}

#[tokio::test]
async fn a_proposal_that_needs_approval_asks_and_decides_nothing() {
    let world = World::new(
        Setup::new(vec![call_tools(vec![(
            "c1",
            "sdr_propose",
            propose_decision(true),
        )])])
        .autonomy(committed_autonomy()),
    )
    .await;
    let ran = world.run(job_with(|_| {})).await;
    assert_eq!(ran.outcome().stop_reason, StopReason::ApprovalRequired);
    assert_eq!(world.head().await, 1);
    assert!(
        ran.transitions().is_empty(),
        "no sdr_transition until it is decided"
    );
    assert_eq!(ran.count("approval_requested"), 1);
    assert!(gate_decisions(&ran).contains(&("approval.sdr_commit".to_owned(), GateDecision::Ask)));
    // The checks ran and their evidence is the run's.
    assert!(ran.count("check_completed") > 0);
    assert!(!ran.outcome().result.evidence_ids.is_empty());
}

#[tokio::test]
async fn arguments_that_break_the_request_schema_are_an_invalid_input_result() {
    let mut harness_fields = propose_mass(MASS_G, &[], false);
    harness_fields["base_version"] = json!(0);
    harness_fields["id"] = json!("tr-chosen-by-the-model");
    let world = World::new(Setup::new(then_give_up(vec![call_tools(vec![(
        "c1",
        "sdr_propose",
        harness_fields,
    )])])))
    .await;
    let ran = world.run(job_with(|_| {})).await;
    assert!(ran.transitions().is_empty());
    assert_eq!(ran.count("check_completed"), 0);
    let [seen] = results_after(&world, 0).try_into().expect("one result");
    assert!(seen.is_error);
    let ToolResultContent::Error { error } = &seen.content else {
        panic!("expected an error, got {:?}", seen.content);
    };
    assert_eq!(error.code.as_str(), "invalid_input");
    assert!(
        error.message.as_str().contains("base_version"),
        "{}",
        error.message.as_str()
    );
}
