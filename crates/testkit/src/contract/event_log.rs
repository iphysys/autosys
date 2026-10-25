use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use contracts::ids::RunId;
use contracts::types::{Event, EventPayload};
use contracts::{ErrorCode, EventLog, UnfencedReason};

use super::{WAIT, assert_code, cancelled_ctx, ctx, unfenced};
use crate::builders::{id, marker_payload, new_event};

/// The `EventLog` contract. `make` builds an empty log. Its markers are
/// `cancel_requested` events, appended unfenced as client commands; the
/// fencing suite covers fenced appends.
pub async fn event_log<S, F, Fut>(make: F)
where
    S: EventLog + 'static,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    sequence_starts_at_one(make().await).await;
    runs_are_independent(make().await).await;
    concurrent_appends_are_gap_free(Arc::new(make().await)).await;
    reading_resumes_after_a_sequence_number(make().await).await;
    wait_for_resolves_on_append(Arc::new(make().await)).await;
    wait_for_honors_cancellation(Arc::new(make().await)).await;
}

fn run(name: &str) -> RunId {
    id(name)
}

async fn append_marker<S: EventLog + ?Sized>(log: &S, run_id: &str, marker: usize) -> Event {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    log.append(
        new_event(run_id, at, marker_payload(marker)),
        Some(&unfenced(UnfencedReason::ApiCommand)),
    )
    .await
    .expect("append")
}

fn marker_of(event: &Event) -> String {
    match &event.payload {
        EventPayload::CancelRequested { reason } => reason
            .as_ref()
            .map(|r| r.as_str().to_owned())
            .unwrap_or_default(),
        other => panic!("unexpected payload {other:?}"),
    }
}

fn seqs(events: &[Event]) -> Vec<u64> {
    events.iter().map(|e| e.seq.get()).collect()
}

async fn sequence_starts_at_one<S: EventLog>(log: S) {
    let run_id = run("run-1");
    assert_eq!(
        log.last_seq(&run_id).await.expect("last_seq"),
        0,
        "sequence_starts_at_one: empty run"
    );
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let new = new_event("run-1", at, marker_payload(0));
    let stored = log
        .append(new.clone(), Some(&unfenced(UnfencedReason::ApiCommand)))
        .await
        .expect("append");
    assert_eq!(stored.seq.get(), 1, "sequence_starts_at_one: first seq");
    assert_eq!(stored.run_id, new.run_id, "sequence_starts_at_one: run id");
    assert_eq!(
        stored.payload, new.payload,
        "sequence_starts_at_one: payload"
    );
    assert_eq!(stored.actor, new.actor, "sequence_starts_at_one: actor");
    assert_eq!(
        stored.occurred_at, new.occurred_at,
        "sequence_starts_at_one: time"
    );
    assert_eq!(
        stored.correlation_id, new.correlation_id,
        "sequence_starts_at_one: correlation"
    );
    for marker in 1..3 {
        append_marker(&log, "run-1", marker).await;
    }
    assert_eq!(
        log.last_seq(&run_id).await.expect("last_seq"),
        3,
        "sequence_starts_at_one: last_seq"
    );
    let all = log.read(&run_id, 0, 100).await.expect("read");
    assert_eq!(seqs(&all), [1, 2, 3], "sequence_starts_at_one: read");
}

async fn runs_are_independent<S: EventLog>(log: S) {
    append_marker(&log, "run-a", 0).await;
    append_marker(&log, "run-a", 1).await;
    let first_b = append_marker(&log, "run-b", 0).await;
    assert_eq!(
        first_b.seq.get(),
        1,
        "runs_are_independent: second run starts at 1"
    );
    let a = log.read(&run("run-a"), 0, 100).await.expect("read");
    assert_eq!(seqs(&a), [1, 2], "runs_are_independent: run-a");
}

async fn concurrent_appends_are_gap_free<S: EventLog + 'static>(log: Arc<S>) {
    const APPENDS: usize = 64;
    let mut handles = Vec::new();
    for marker in 0..APPENDS {
        let log = Arc::clone(&log);
        handles.push(tokio::spawn(async move {
            append_marker(log.as_ref(), "run-1", marker).await.seq.get()
        }));
    }
    let mut assigned = Vec::new();
    for handle in handles {
        assigned.push(handle.await.expect("concurrent_appends: task panicked"));
    }
    assigned.sort_unstable();
    let expected: Vec<u64> = (1..=APPENDS as u64).collect();
    assert_eq!(
        assigned, expected,
        "concurrent_appends: sequence numbers have gaps or repeats"
    );

    let stored = log.read(&run("run-1"), 0, 1000).await.expect("read");
    assert_eq!(seqs(&stored), expected, "concurrent_appends: read order");
    let markers: HashSet<String> = stored.iter().map(marker_of).collect();
    assert_eq!(
        markers.len(),
        APPENDS,
        "concurrent_appends: an event was lost or duplicated"
    );
}

async fn reading_resumes_after_a_sequence_number<S: EventLog>(log: S) {
    let run_id = run("run-1");
    for marker in 0..10 {
        append_marker(&log, "run-1", marker).await;
    }
    let after_four = log.read(&run_id, 4, 100).await.expect("read");
    assert_eq!(
        seqs(&after_four),
        [5, 6, 7, 8, 9, 10],
        "reading_resumes: after 4"
    );
    let limited = log.read(&run_id, 4, 3).await.expect("read");
    assert_eq!(seqs(&limited), [5, 6, 7], "reading_resumes: limit");
    let none = log.read(&run_id, 10, 100).await.expect("read");
    assert!(
        none.is_empty(),
        "reading_resumes: events after the last one"
    );

    // A client reading in pages and resuming from its last seq sees every
    // event exactly once.
    let mut seen = Vec::new();
    let mut last = 0;
    loop {
        let page = log.read(&run_id, last, 3).await.expect("read");
        let Some(tail) = page.last() else { break };
        last = tail.seq.get();
        seen.extend(seqs(&page));
    }
    assert_eq!(
        seen,
        (1..=10).collect::<Vec<u64>>(),
        "reading_resumes: paged read"
    );
}

async fn wait_for_resolves_on_append<S: EventLog + 'static>(log: Arc<S>) {
    let waiter = {
        let log = Arc::clone(&log);
        tokio::spawn(async move { log.wait_for(&run("run-1"), 0, &ctx()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished(), "wait_for: resolved before any event");
    append_marker(log.as_ref(), "run-1", 0).await;
    tokio::time::timeout(WAIT, waiter)
        .await
        .expect("wait_for: did not resolve after an append")
        .expect("wait_for: task panicked")
        .expect("wait_for: returned an error");

    // Already satisfied: resolves at once.
    tokio::time::timeout(WAIT, log.wait_for(&run("run-1"), 0, &ctx()))
        .await
        .expect("wait_for: did not resolve for an existing event")
        .expect("wait_for: returned an error");
}

async fn wait_for_honors_cancellation<S: EventLog + 'static>(log: Arc<S>) {
    let result = tokio::time::timeout(WAIT, log.wait_for(&run("run-1"), 0, &cancelled_ctx()))
        .await
        .expect("wait_for: ignored an already cancelled context");
    assert_code(
        result,
        ErrorCode::Cancelled,
        "wait_for_honors_cancellation: pre-cancelled",
    );

    let context = ctx();
    let waiter = {
        let log = Arc::clone(&log);
        let context = context.clone();
        tokio::spawn(async move { log.wait_for(&run("run-1"), 0, &context).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    context.cancel();
    let result = tokio::time::timeout(WAIT, waiter)
        .await
        .expect("wait_for: ignored cancellation while waiting")
        .expect("wait_for: task panicked");
    assert_code(
        result,
        ErrorCode::Cancelled,
        "wait_for_honors_cancellation: while waiting",
    );
}
