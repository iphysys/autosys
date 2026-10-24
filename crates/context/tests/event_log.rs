//! The context's hash goes into the event log (blueprint section 8), and a
//! context that is not recorded is never handed out.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::sync::Arc;

use context::ContextError;
use contracts::ErrorCode;
use contracts::types::{Actor, EventPayload};
use serde_json::json;
use support::*;

async fn populated() -> World {
    let world = World::new().await;
    world
        .commit(vec![
            add_requirement("R-001", "The plate mass shall not exceed 1 kg.", 1.0),
            set_parameter("part.mass", 0.84, "kg"),
        ])
        .await;
    world
        .event(json!({ "type": "run_started", "task_id": "task-1", "agent_id": "orchestrator" }))
        .await;
    world
        .event(json!({
            "type": "check_completed", "check_id": "units", "evidence_id": "ev-1", "verdict": "pass"
        }))
        .await;
    world
}

#[tokio::test]
async fn one_build_records_exactly_one_matching_event() {
    let world = populated().await;
    let open_turn = tool_round("mass = 0.84 kg");
    let context = world.build(&job(), &open_turn).await.expect("builds");

    let recorded = world.events_of("context_assembled").await;
    assert_eq!(recorded.len(), 1, "exactly one event");
    let event = &recorded[0];
    let EventPayload::ContextAssembled {
        context_hash,
        total_tokens,
        sdr_version,
        last_event_seq,
    } = &event.payload
    else {
        panic!("not a context_assembled event");
    };
    assert_eq!(context_hash, context.hash());
    assert_eq!(*total_tokens, context.total_tokens());
    assert_eq!(sdr_version.0, 1, "the head at the time");
    assert_eq!(*last_event_seq, 2, "the last event the context could see");
    assert_eq!(context.event_seq(), Some(event.seq.get()));
    assert_eq!(context.manifest().sdr_version, 1);
    assert_eq!(context.manifest().last_event_seq, 2);

    assert_eq!(event.run_id.as_str(), RUN);
    assert_eq!(
        event.correlation_id.as_str(),
        world.ctx.correlation_id().as_str()
    );
    assert_eq!(
        event.actor,
        Actor::System {
            component: "context".parse().expect("identifier")
        }
    );
    // The time is the injected clock's.
    assert_eq!(event.occurred_at.0, contracts::Clock::now(&*world.clock));
}

#[tokio::test]
async fn every_build_records_its_own_event_even_with_the_same_hash() {
    // After a restart the context is built again from the same sources. The
    // second event has the same hash; that is expected, not a gap.
    let world = populated().await;
    let first = world.build(&job(), &[]).await.expect("builds");
    let second = world.build(&job(), &[]).await.expect("builds");
    assert_eq!(first.hash(), second.hash());
    let recorded = world.events_of("context_assembled").await;
    assert_eq!(recorded.len(), 2);
    assert_ne!(first.event_seq(), second.event_seq());
}

#[tokio::test]
async fn a_rebuild_at_the_recorded_snapshot_gives_the_same_hash() {
    let world = populated().await;
    let open_turn = tool_round("mass = 0.84 kg");
    let original = world.build(&job(), &open_turn).await.expect("builds");

    // The recorded snapshot, read back from the event as a replay would.
    let recorded = world.events_of("context_assembled").await;
    let EventPayload::ContextAssembled {
        sdr_version,
        last_event_seq,
        ..
    } = &recorded[0].payload
    else {
        panic!("not a context_assembled event");
    };
    let snapshot = pinned(sdr_version.0, *last_event_seq);

    // The world moves on: a new design version and new events.
    world
        .commit(vec![set_parameter("part.mass", 0.5, "kg")])
        .await;
    world
        .event(json!({
            "type": "check_completed", "check_id": "units", "evidence_id": "ev-2", "verdict": "fail"
        }))
        .await;
    world
        .event(json!({ "type": "cancel_requested", "reason": "later" }))
        .await;

    let latest = world.build(&job(), &open_turn).await.expect("builds");
    assert_ne!(
        latest.hash(),
        original.hash(),
        "the design and the log moved on"
    );

    let job = job();
    let mut replay = request(&job, &open_turn);
    replay.at = Some(snapshot);
    let replayed = world.build_with(replay).await.expect("builds");
    assert_eq!(replayed.hash(), original.hash());
    assert_eq!(replayed.manifest(), original.manifest());
    assert_eq!(replayed.messages(), original.messages());
}

#[tokio::test]
async fn a_pinned_snapshot_keeps_one_view_of_the_design_across_a_turn() {
    let world = populated().await;
    let job = job();
    let mut first = request(&job, &[]);
    first.at = Some(pinned(1, 2));
    let before = world.build_with(first).await.expect("builds");

    world
        .commit(vec![set_parameter("part.mass", 0.5, "kg")])
        .await;
    let mut second = request(&job, &[]);
    second.at = Some(pinned(1, 2));
    let after = world.build_with(second).await.expect("builds");
    assert_eq!(before.hash(), after.hash());
    assert!(after.messages()[0].content.iter().any(|b| matches!(
        b,
        contracts::ContentBlock::Text { text } if text.contains("version=\"1\"")
    )));
}

#[tokio::test]
async fn a_failed_append_fails_the_build_and_hands_out_nothing() {
    let world = populated().await;
    let mut flaky = FlakyEvents::new(world.events.clone());
    flaky.append = Behaviour::Fail(ErrorCode::Unavailable);
    let builder = world.builder_over(Arc::new(flaky));

    let job = job();
    let result = builder.build(request(&job, &[]), &world.ctx).await;
    match result {
        Err(ContextError::Events(error)) => assert_eq!(error.code(), ErrorCode::Unavailable),
        other => panic!("expected an event log error, got {:?}", other.map(|_| ())),
    }
    assert!(world.events_of("context_assembled").await.is_empty());

    // The error maps to the log's own code, retryable like the log's.
    let again = builder.build(request(&job, &[]), &world.ctx).await;
    let error = again.expect_err("still failing").into_error(&world.ctx);
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.retryable());
}

#[tokio::test]
async fn the_history_is_the_latest_window_of_the_log() {
    // A run with more events than the window: the history is the latest ones.
    let world = populated().await;
    for n in 0..300 {
        world
            .event(json!({
                "type": "check_completed", "check_id": "units",
                "evidence_id": format!("ev-{n}"), "verdict": "pass"
            }))
            .await;
    }
    let context = world.build(&job(), &[]).await.expect("builds");
    let history = context
        .manifest()
        .sections
        .iter()
        .find(|s| s.name == contracts::types::ContextSectionName::RunHistory)
        .expect("section");
    // The window is the latest 200 events, 103 to 302; the first 102 are
    // never read. The section's own cap then trims the oldest of the window
    // and says which: the omission starts where the window does.
    let omitted = history.omitted.as_ref().expect("the section was trimmed");
    assert_eq!(omitted.first_seq, Some(103), "{history:?}");
    assert_eq!(
        history.items.last().map(String::as_str),
        Some("event:302"),
        "the newest event is kept"
    );
    let kept = u64::try_from(history.items.len()).expect("small");
    assert_eq!(
        omitted.items + kept,
        200,
        "kept and omitted make the window"
    );

    let recorded = world.events_of("context_assembled").await;
    let EventPayload::ContextAssembled { last_event_seq, .. } = &recorded[0].payload else {
        panic!("not a context_assembled event");
    };
    assert_eq!(*last_event_seq, 302);
}
