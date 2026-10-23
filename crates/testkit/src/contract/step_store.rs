use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use contracts::ids::Identifier;
use contracts::types::Timestamp;
use contracts::{Clock, ErrorCode, StepStart, StepStore};
use serde_json::json;

use super::assert_code;
use crate::builders::{failed, id, new_step, succeeded};
use crate::clock::ManualClock;

struct Fixture<S> {
    clock: Arc<ManualClock>,
    store: Arc<S>,
}

async fn fresh<S, F, Fut>(make: &F) -> Fixture<S>
where
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = S>,
{
    let clock = Arc::new(ManualClock::at_epoch());
    let store = Arc::new(make(clock.clone()).await);
    Fixture { clock, store }
}

/// The `StepStore` contract. `make` builds an empty store that reads time
/// from the given clock.
pub async fn step_store<S, F, Fut>(make: F)
where
    S: StepStore + 'static,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = S>,
{
    begin_then_get(fresh(&make).await).await;
    begin_again_returns_the_existing_record(fresh(&make).await).await;
    a_reused_key_for_another_request_conflicts(fresh(&make).await).await;
    complete_then_replay(fresh(&make).await).await;
    complete_is_idempotent_and_final(fresh(&make).await).await;
    unknown_keys_are_not_found(fresh(&make).await).await;
    concurrent_begins_start_once(fresh(&make).await).await;
}

fn started(start: StepStart, case: &str) -> contracts::types::StepRecord {
    match start {
        StepStart::Started(record) => record,
        StepStart::Existing(record) => panic!("{case}: expected Started, got Existing({record:?})"),
    }
}

fn existing(start: StepStart, case: &str) -> contracts::types::StepRecord {
    match start {
        StepStart::Existing(record) => record,
        StepStart::Started(record) => panic!("{case}: expected Existing, got Started({record:?})"),
    }
}

async fn begin_then_get<S: StepStore>(f: Fixture<S>) {
    let step = new_step("step-1", "create_plate", "request-a");
    let record = started(f.store.begin(&step).await.expect("begin"), "begin_then_get");
    assert_eq!(
        record.idempotency_key, step.idempotency_key,
        "begin_then_get: key"
    );
    assert_eq!(record.run_id, step.run_id, "begin_then_get: run");
    assert_eq!(record.step_id, step.step_id, "begin_then_get: step");
    assert_eq!(record.tool, step.tool, "begin_then_get: tool");
    assert_eq!(
        record.request_sha256, step.request_sha256,
        "begin_then_get: hash"
    );
    assert_eq!(
        record.started_at,
        Timestamp(f.clock.now()),
        "begin_then_get: started_at"
    );
    assert!(
        record.outcome.is_none() && record.completed_at.is_none(),
        "begin_then_get: a started step has no outcome"
    );
    let stored = f.store.get(&step.idempotency_key).await.expect("get");
    assert_eq!(stored, record, "begin_then_get: stored record differs");
}

async fn begin_again_returns_the_existing_record<S: StepStore>(f: Fixture<S>) {
    let step = new_step("step-1", "create_plate", "request-a");
    let first = started(f.store.begin(&step).await.expect("begin"), "begin again");
    f.clock.advance(Duration::from_secs(5));
    let again = existing(
        f.store.begin(&step).await.expect("begin again"),
        "begin again",
    );
    assert_eq!(again, first, "begin again: the record changed");
}

async fn a_reused_key_for_another_request_conflicts<S: StepStore>(f: Fixture<S>) {
    let step = new_step("step-1", "create_plate", "request-a");
    let first = started(f.store.begin(&step).await.expect("begin"), "reused key");
    let other_request = new_step("step-1", "create_plate", "request-b");
    assert_code(
        f.store.begin(&other_request).await,
        ErrorCode::Conflict,
        "reused key: different arguments",
    );
    let other_tool = new_step("step-1", "measure", "request-a");
    assert_code(
        f.store.begin(&other_tool).await,
        ErrorCode::Conflict,
        "reused key: different tool",
    );
    let mut other_step = step.clone();
    other_step.step_id = id("step-2");
    assert_code(
        f.store.begin(&other_step).await,
        ErrorCode::Conflict,
        "reused key: different step",
    );
    let stored = f.store.get(&step.idempotency_key).await.expect("get");
    assert_eq!(stored, first, "reused key: the record was overwritten");
}

async fn complete_then_replay<S: StepStore>(f: Fixture<S>) {
    let step = new_step("step-1", "measure", "request-a");
    let begun = started(f.store.begin(&step).await.expect("begin"), "complete");
    f.clock.advance(Duration::from_secs(2));
    let outcome = succeeded(json!({ "mass": { "value": 122.5, "unit": "g" } }));
    let completed = f
        .store
        .complete(&step.idempotency_key, &outcome)
        .await
        .expect("complete");
    assert_eq!(
        completed.outcome.as_ref(),
        Some(&outcome),
        "complete: outcome"
    );
    assert_eq!(
        completed.completed_at,
        Some(Timestamp(f.clock.now())),
        "complete: completed_at"
    );
    assert_eq!(
        completed.started_at, begun.started_at,
        "complete: started_at changed"
    );
    let replayed = existing(f.store.begin(&step).await.expect("replay"), "replay");
    assert_eq!(
        replayed, completed,
        "replay: begin did not return the completed record"
    );
    let stored = f.store.get(&step.idempotency_key).await.expect("get");
    assert_eq!(stored, completed, "complete: stored record differs");
}

async fn complete_is_idempotent_and_final<S: StepStore>(f: Fixture<S>) {
    let step = new_step("step-1", "create_plate", "request-a");
    f.store.begin(&step).await.expect("begin");
    let outcome = failed("timeout");
    let first = f
        .store
        .complete(&step.idempotency_key, &outcome)
        .await
        .expect("complete");
    f.clock.advance(Duration::from_secs(5));
    let again = f
        .store
        .complete(&step.idempotency_key, &outcome)
        .await
        .expect("complete again with the same outcome");
    assert_eq!(again, first, "complete again: the record changed");
    assert_code(
        f.store
            .complete(&step.idempotency_key, &succeeded(json!({})))
            .await,
        ErrorCode::Conflict,
        "complete: a different outcome",
    );
    let stored = f.store.get(&step.idempotency_key).await.expect("get");
    assert_eq!(stored, first, "complete: the outcome was overwritten");
}

async fn unknown_keys_are_not_found<S: StepStore>(f: Fixture<S>) {
    let missing: Identifier = id("run-1:step-missing");
    assert_code(
        f.store.get(&missing).await,
        ErrorCode::NotFound,
        "get unknown",
    );
    assert_code(
        f.store.complete(&missing, &failed("timeout")).await,
        ErrorCode::NotFound,
        "complete unknown",
    );
}

async fn concurrent_begins_start_once<S: StepStore + 'static>(f: Fixture<S>) {
    let step = new_step("step-1", "create_plate", "request-a");
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let step = step.clone();
        handles.push(tokio::spawn(async move { store.begin(&step).await }));
    }
    let mut started_count = 0;
    for handle in handles {
        match handle.await.expect("task").expect("concurrent begin") {
            StepStart::Started(_) => started_count += 1,
            StepStart::Existing(_) => {}
        }
    }
    assert_eq!(
        started_count, 1,
        "concurrent begins: {started_count} started the step"
    );
}
