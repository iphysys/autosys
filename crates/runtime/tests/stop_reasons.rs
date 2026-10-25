//! PR 11's done-when: with the fake model and the fake tools, scripted runs
//! reach every stop reason; a model claiming success without passing checks
//! does not stop with `success`; an exhausted budget ends the run with
//! `budget_exhausted`.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use contracts::ErrorCode;
use contracts::types::{ApprovalAnswer, EventPayload, StopReason};
use serde_json::{Value, json};
use support::*;
use testkit::builders::parse;

/// create_plate, then measure, then propose the measured mass.
fn measure_and_propose(is_final: bool) -> Vec<ScriptStep> {
    vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        call_tools(vec![("c2", "measure", measure_args())]),
        from_request(move |request| {
            let evidence = last_evidence(request);
            assert_eq!(evidence.len(), 1, "measure returns one evidence id");
            tool_use(&[(
                "c3".to_owned(),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, is_final),
            )])
        }),
    ]
}

fn job() -> contracts::types::Job {
    job_for(task_json(&["R-mass"], roomy_budget()))
}

fn job_with_budget(budget: Value) -> contracts::types::Job {
    job_for(task_json(&["R-mass"], budget))
}

// --------------------------------------------------------------- success ---

#[tokio::test]
async fn success_after_the_approval_policy_asks_and_a_human_approves() {
    let world =
        World::new(Setup::new(measure_and_propose(true)).autonomy(committed_autonomy())).await;
    let first = world.run(job()).await;
    let waiting = first.outcome();
    assert_eq!(waiting.stop_reason, StopReason::ApprovalRequired);
    assert_eq!(world.head().await, 1, "nothing is committed while it waits");
    let continuation = waiting.continuation.clone().expect("a continuation");

    let resumed = world.resume(continuation, ApprovalAnswer::Approve).await;
    let outcome = resumed.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Success);
    assert_eq!(world.head().await, 2);
    assert_eq!(outcome.result.stop_reason, StopReason::Success);
    assert_eq!(outcome.result.sdr_version.as_ref().map(|v| v.0), Some(2));
    assert_eq!(outcome.result.committed_transition_ids.len(), 1);
    assert_eq!(outcome.job.stop_reason, Some(StopReason::Success));
    assert!(outcome.continuation.is_none());
    // The resumed stream continues where the first stopped.
    assert_eq!(
        resumed.events.first().map(|e| e.seq.get()),
        first.events.last().map(|e| e.seq.get() + 1)
    );
    assert_eq!(
        resumed.kinds().iter().rev().take(2).collect::<Vec<_>>(),
        [&"run_stopped", &"sdr_transition"]
    );
    assert_eq!(
        world.model.remaining(),
        0,
        "no model call after the approval"
    );
}

#[tokio::test]
async fn success_without_an_approval_policy_stops_right_after_the_final_commit() {
    let world = World::new(Setup::new(measure_and_propose(true))).await;
    let ran = world.run(job()).await;
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Success);
    assert!(outcome.result.error.is_none());
    assert_eq!(world.head().await, 2);
    assert_eq!(ran.kinds().last(), Some(&"run_stopped"));
    let transitions = ran.transitions();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].1, "committed");
    assert_eq!(transitions[0].2, Some(2));
    assert!(
        outcome.job.pending_work.is_empty(),
        "{:?}",
        outcome.job.pending_work
    );
    assert_eq!(world.model.requests().len(), 3);
}

// --------------------------------------------------------------- failure ---

#[tokio::test]
async fn repeated_give_ups_end_in_failure_after_the_repair_bound() {
    let world = World::new(Setup::new(vec![
        reply("I cannot make progress."),
        reply("Still stuck."),
        reply("Giving up."),
        reply("Never asked."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 2)))
        .await;
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Failure);
    let errors = ran.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, "repair.exhausted");
    assert_eq!(
        outcome.result.error.as_ref().map(|e| e.code.as_str()),
        Some("repair.exhausted")
    );
    assert_eq!(
        world.model.requests().len(),
        3,
        "the third give-up is one beyond the bound"
    );
    assert_eq!(world.model.remaining(), 1);
}

#[tokio::test]
async fn repeated_rejections_end_in_failure_and_commit_nothing() {
    let propose = || {
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[(
                format!("p{}", request.messages.len()),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, false),
            )])
        })
    };
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![("c1", "measure", measure_args())]),
            propose(),
            propose(),
            propose(),
        ])
        // The plate weighs about 122.6 g, so R-mass fails every time.
        .requirements(vec![mass_requirement("R-mass", 100.0)]),
    )
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 2)))
        .await;
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Failure);
    assert_eq!(world.head().await, 1, "nothing committed");
    let transitions = ran.transitions();
    assert_eq!(transitions.len(), 3);
    assert!(
        transitions.iter().all(|t| t.1 == "rejected"),
        "{transitions:?}"
    );
    assert!(
        transitions[0]
            .3
            .iter()
            .any(|r| r.contains("requirement.numeric_threshold failed")),
        "{transitions:?}"
    );
    assert_eq!(
        ran.errors().last().map(|e| e.0.as_str()),
        Some("repair.exhausted")
    );
}

// ------------------------------------------------- claims are not success ---

#[tokio::test]
async fn a_text_claim_of_success_ends_as_failure_never_success() {
    let world = World::new(Setup::new(vec![
        reply("All requirements are met. The task succeeded."),
        reply("All requirements are met. The task succeeded."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 1)))
        .await;
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Failure);
    assert_ne!(outcome.result.stop_reason, StopReason::Success);
    assert_eq!(world.head().await, 1);
    // Each claim was answered with what is still unmet.
    let pending: Vec<&str> = outcome
        .job
        .pending_work
        .iter()
        .map(|w| w.as_str())
        .collect();
    assert_eq!(
        pending,
        ["requirement R-mass: no passing check at sdr version 1"]
    );
    let second = &world.model.requests()[1];
    assert!(
        context_text(second).contains("requirement R-mass: no passing check at sdr version 1"),
        "the job state shows the pending work"
    );
}

#[tokio::test]
async fn a_final_commit_with_an_unverified_requirement_does_not_succeed() {
    // The proposal sets no mass, so R-mass is unverified: the commit gate
    // lets it through, but unverified never counts as passing.
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "sdr_propose", propose_decision(true))]),
        reply("All requirements are met."),
        reply("All requirements are met."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 1)))
        .await;
    let outcome = ran.outcome();
    assert_eq!(world.head().await, 2, "the final design committed");
    assert_eq!(ran.transitions()[0].1, "committed");
    assert!(
        ran.events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::CheckCompleted { check_id, verdict, .. }
                if check_id.as_str() == "requirement.numeric_threshold"
                    && *verdict == contracts::types::Verdict::Unverified
        )),
        "R-mass was unverified"
    );
    assert_ne!(outcome.stop_reason, StopReason::Success);
    assert_eq!(outcome.stop_reason, StopReason::Failure);
    assert_eq!(
        world.model.requests().len(),
        3,
        "the run went on after the commit"
    );
    let pending: Vec<&str> = outcome
        .job
        .pending_work
        .iter()
        .map(|w| w.as_str())
        .collect();
    assert_eq!(
        pending,
        ["requirement R-mass: no passing check at sdr version 2"]
    );
}

#[tokio::test]
async fn a_non_final_commit_with_every_requirement_passing_does_not_succeed() {
    let mut script = measure_and_propose(false);
    script.push(reply("Done."));
    script.push(reply("Done."));
    let world = World::new(Setup::new(script)).await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 1)))
        .await;
    let outcome = ran.outcome();
    assert_eq!(world.head().await, 2);
    assert_eq!(outcome.stop_reason, StopReason::Failure);
    let pending: Vec<&str> = outcome
        .job
        .pending_work
        .iter()
        .map(|w| w.as_str())
        .collect();
    assert_eq!(pending, ["final design: not committed at sdr version 2"]);
}

// ------------------------------------------------------ budget_exhausted ---

/// The run stopped with `budget_exhausted`, and the error it recorded has
/// `code`.
fn assert_budget_exhausted(ran: &Ran, code: &str) {
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    let errors = ran.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, code, "{errors:?}");
    assert_eq!(
        outcome.result.error.as_ref().map(|e| e.code.as_str()),
        Some(code)
    );
    assert!(outcome.result.error.as_ref().is_some_and(|e| !e.retryable));
    assert_eq!(
        ran.kinds().iter().rev().take(2).collect::<Vec<_>>(),
        [&"run_stopped", &"error"]
    );
}

#[tokio::test]
async fn an_exhausted_token_budget_stops_before_the_next_call() {
    let world = World::new(Setup::new(vec![
        with_usage(
            call_tools(vec![("c1", "sdr_read", json!({}))]),
            15_000,
            1_000,
        ),
        reply("Never asked."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(20_000, 20, 20, 600, 3)))
        .await;
    assert_budget_exhausted(&ran, "budget.tokens_exhausted");
    assert_eq!(world.model.requests().len(), 1);
    assert_eq!(world.model.remaining(), 1, "no further model call");
    let used = &ran.outcome().result.usage;
    assert_eq!((used.input_tokens, used.output_tokens), (15_000, 1_000));
    // The first call could not ask for more output than the budget left.
    let first = &world.model.requests()[0];
    assert!(
        first.max_output_tokens < 16_000,
        "{}",
        first.max_output_tokens
    );
}

#[tokio::test]
async fn an_exhausted_tool_call_budget_stops_before_the_call() {
    let world = World::new(Setup::new(vec![
        call_tools(vec![
            ("c1", "create_plate", plate()),
            ("c2", "create_plate", plate()),
            ("c3", "create_plate", plate()),
        ]),
        reply("Never asked."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 2, 20, 600, 3)))
        .await;
    assert_budget_exhausted(&ran, "budget.tool_calls_exhausted");
    assert_eq!(world.create_plate.calls(), 2, "the third call did not run");
    assert_eq!(ran.outcome().result.usage.tool_calls, 2);
    assert_eq!(world.model.remaining(), 1, "no further model call");
}

#[tokio::test]
async fn a_spent_tool_call_budget_stops_before_the_next_model_call() {
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "sdr_read", json!({}))]),
        reply("Never asked."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 1, 20, 600, 3)))
        .await;
    assert_budget_exhausted(&ran, "budget.tool_calls_exhausted");
    assert_eq!(world.model.requests().len(), 1);
    assert_eq!(
        world.model.remaining(),
        1,
        "no model call without a tool call left"
    );
}

#[tokio::test]
async fn an_exhausted_step_budget_stops_before_the_next_call() {
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "sdr_read", json!({}))]),
        call_tools(vec![("c2", "sdr_read", json!({}))]),
        reply("Never asked."),
    ]))
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 2, 600, 3)))
        .await;
    assert_budget_exhausted(&ran, "budget.steps_exhausted");
    assert_eq!(world.model.requests().len(), 2);
    assert_eq!(world.model.remaining(), 1, "no further model call");
    assert_eq!(ran.outcome().job.progress.steps_completed, 2);
}

#[tokio::test(start_paused = true)]
async fn an_exhausted_wall_time_budget_cancels_the_call_in_flight() {
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![
                ("c1", "measure", measure_args()),
                ("c2", "create_plate", plate()),
            ]),
            reply("Never asked."),
        ])
        .measure(returning("measure", measure_result()).with_delay(SLOW)),
    )
    .await;
    let ran = world
        .run(job_with_budget(budget(200_000, 20, 20, 30, 3)))
        .await;
    assert_budget_exhausted(&ran, "budget.wall_time_exhausted");
    assert_eq!(world.measure.calls(), 1);
    assert_eq!(world.create_plate.calls(), 0, "no further tool call");
    assert_eq!(world.model.remaining(), 1, "no further model call");
    // The call in flight ended as cancelled, and the gateway recorded it.
    assert!(ran.events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::ToolCallCompleted { error: Some(error), .. }
            if error.code.as_str() == ErrorCode::Cancelled.as_str()
    )));
    let used = ran.outcome().result.usage.wall_time_seconds;
    assert!((30.0..31.0).contains(&used), "{used}");
}

#[tokio::test]
async fn a_context_that_does_not_fit_stops_with_context_overflow() {
    // The design state is never cut, and this requirement alone is above
    // its 8000-token cap.
    let huge = "The plate shall be documented. ".repeat(1_000);
    let world = World::new(
        Setup::new(vec![reply("Never asked.")])
            .requirements(vec![text_requirement("R-doc", "must", &huge)]),
    )
    .await;
    let ran = world
        .run(job_for(task_json(&["R-doc"], roomy_budget())))
        .await;
    assert_budget_exhausted(&ran, "context_overflow");
    assert!(
        ran.errors()[0].1.contains("design_state"),
        "{:?}",
        ran.errors()
    );
    assert!(world.model.requests().is_empty(), "no model call");
    assert_eq!(ran.count("context_assembled"), 0);
}

// ------------------------------------------------------------- cancelled ---

#[tokio::test(start_paused = true)]
async fn cancelling_during_a_slow_tool_stops_with_cancelled() {
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![
                ("c1", "measure", measure_args()),
                ("c2", "create_plate", plate()),
            ]),
            reply("Never asked."),
        ])
        .measure(returning("measure", measure_result()).with_delay(SLOW)),
    )
    .await;
    let run_ctx = world.ctx();
    let canceller = run_ctx.clone();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        canceller.cancel();
    });
    let ran = world.run_with(job(), run_ctx).await;
    cancel.await.expect("the canceller ran");
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Cancelled);
    assert_eq!(outcome.job.stop_reason, Some(StopReason::Cancelled));
    assert!(ran.errors().is_empty(), "a cancel is not an error");
    assert_eq!(ran.kinds().last(), Some(&"run_stopped"));
    assert_eq!(world.create_plate.calls(), 0);
    assert_eq!(world.model.remaining(), 1);
}

// ----------------------------------------------------- approval_required ---

#[tokio::test]
async fn the_committed_autonomy_policy_stops_a_final_commit_with_approval_required() {
    let world =
        World::new(Setup::new(measure_and_propose(true)).autonomy(committed_autonomy())).await;
    let ran = world.run(job()).await;
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::ApprovalRequired);
    assert_eq!(outcome.result.stop_reason, StopReason::ApprovalRequired);
    assert_eq!(
        outcome.job.stop_reason, None,
        "waiting is not a terminal state"
    );
    assert!(ran.transitions().is_empty(), "nothing was decided");
    assert_eq!(world.head().await, 1);

    let requested: Vec<String> = ran
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ApprovalRequested { approval_id, .. } => {
                Some(approval_id.as_str().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(requested.len(), 1);
    let continuation = outcome.continuation.as_ref().expect("a continuation");
    assert_eq!(continuation.approval_id().as_str(), requested[0]);
    assert_eq!(continuation.job(), &outcome.job);
    assert_eq!(ran.kinds().last(), Some(&"run_stopped"));
    assert!(matches!(
        ran.events.last().map(|e| &e.payload),
        Some(EventPayload::RunStopped {
            stop_reason: StopReason::ApprovalRequired,
            ..
        })
    ));
}

#[tokio::test]
async fn a_refused_approval_rejects_the_commit_and_the_run_goes_on() {
    let mut script = measure_and_propose(true);
    script.push(reply("Understood."));
    script.push(reply("Understood."));
    let world = World::new(Setup::new(script).autonomy(committed_autonomy())).await;
    let first = world
        .run(job_with_budget(budget(200_000, 20, 20, 600, 1)))
        .await;
    let continuation = first
        .outcome()
        .continuation
        .clone()
        .expect("a continuation");
    let resumed = world.resume(continuation, ApprovalAnswer::Reject).await;
    let outcome = resumed.outcome();
    assert_eq!(world.head().await, 1);
    assert_eq!(resumed.transitions()[0].1, "rejected");
    assert!(
        resumed.transitions()[0]
            .3
            .iter()
            .any(|r| r.contains("a human refused")),
        "{:?}",
        resumed.transitions()
    );
    assert_eq!(outcome.stop_reason, StopReason::Failure);
}

#[tokio::test]
async fn a_tool_that_needs_approval_waits_and_runs_once_approved() {
    let policy = parse(json!({
        "schema_version": "0.1",
        "approvals": [{
            "id": "approve_measure",
            "description": "Ask before measuring.",
            "trigger": { "kind": "tool_call", "tool": "measure" }
        }]
    }));
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![("c1", "measure", measure_args())]),
            from_request(|request| {
                let evidence = last_evidence(request);
                tool_use(&[(
                    "c2".to_owned(),
                    "sdr_propose".to_owned(),
                    propose_mass(MASS_G, &evidence, true),
                )])
            }),
        ])
        .autonomy(policy),
    )
    .await;
    let first = world.run(job()).await;
    assert_eq!(first.outcome().stop_reason, StopReason::ApprovalRequired);
    assert_eq!(world.measure.calls(), 0, "the tool waits for the answer");
    let continuation = first
        .outcome()
        .continuation
        .clone()
        .expect("a continuation");
    let resumed = world.resume(continuation, ApprovalAnswer::Approve).await;
    assert_eq!(world.measure.calls(), 1);
    assert_eq!(resumed.outcome().stop_reason, StopReason::Success);
    // The approved call was counted once, when it first asked.
    assert_eq!(resumed.outcome().result.usage.tool_calls, 2);
}

#[tokio::test]
async fn a_task_above_its_agents_limits_is_a_harness_failure_without_run_stopped() {
    let world = World::new(Setup::new(vec![reply("Never asked.")])).await;
    let ran = world
        .run(job_with_budget(budget(2_000_000, 20, 20, 600, 3)))
        .await;
    let Err(error) = &ran.outcome else {
        panic!("the task was run");
    };
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert_eq!(ran.kinds(), ["error"], "an error event and no run_stopped");
    assert!(world.model.requests().is_empty());
}
