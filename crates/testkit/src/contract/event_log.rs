use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use contracts::ids::RunId;
use contracts::types::{Event, EventPayload};
use contracts::{Clock, ErrorCode, EventKind, EventLog, JobStore, UnfencedReason, WriteAuthority};
use serde_json::{Value, json};

use super::{WAIT, assert_code, cancelled_ctx, ctx, fenced, lease_one, unfenced};
use crate::builders::{id, marker_payload, new_event};
use crate::clock::ManualClock;

/// A log, and the fence of a job claimed for the whole case, for the events
/// no unfenced reason permits.
struct Fenced<S> {
    log: Arc<S>,
    authority: WriteAuthority,
}

impl<S: EventLog> Fenced<S> {
    async fn append(&self, payload: Value) -> Event {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        self.log
            .append(new_event("run-1", at, payload), Some(&self.authority))
            .await
            .expect("fenced append")
    }
}

async fn fenced_log<S, J, F, Fut>(make: &F) -> Fenced<S>
where
    J: JobStore,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = (S, J)>,
{
    let clock = Arc::new(ManualClock::at_epoch());
    let (log, jobs) = make(clock.clone()).await;
    let claimed = lease_one(&jobs, &clock).await;
    Fenced {
        log: Arc::new(log),
        authority: fenced(&claimed),
    }
}

/// The `EventLog` contract. `make` builds an empty log, and a job store
/// whose fences it checks, on the given clock. Its markers are
/// `cancel_requested` events, appended unfenced as client commands; events
/// no unfenced reason permits are appended under a current fence, and the
/// fencing suite covers stale ones.
pub async fn event_log<S, J, F, Fut>(make: F)
where
    S: EventLog + 'static,
    J: JobStore,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = (S, J)>,
{
    let log = || async { make(Arc::new(ManualClock::at_epoch())).await.0 };
    sequence_starts_at_one(log().await).await;
    runs_are_independent(log().await).await;
    concurrent_appends_are_gap_free(Arc::new(log().await)).await;
    reading_resumes_after_a_sequence_number(log().await).await;
    wait_for_resolves_on_append(Arc::new(log().await)).await;
    wait_for_honors_cancellation(Arc::new(log().await)).await;
    completion_events_are_stored_once(fenced_log(&make).await).await;
    concurrent_completions_are_stored_once(fenced_log(&make).await).await;
    reading_by_kind_returns_only_those_kinds(fenced_log(&make).await).await;
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

fn run_started() -> Value {
    json!({ "type": "run_started", "task_id": "task-1", "agent_id": "test_agent" })
}

fn tool_call_completed(step: &str, duration_ms: u64, recovered: Option<bool>) -> Value {
    let mut payload = json!({
        "type": "tool_call_completed", "step_id": step, "tool": "measure",
        "outcome": "ok", "evidence_ids": [], "duration_ms": duration_ms
    });
    if let Some(recovered) = recovered {
        payload["recovered"] = json!(recovered);
    }
    payload
}

fn sdr_transition(transition: &str) -> Value {
    json!({
        "type": "sdr_transition", "transition_id": transition, "outcome": "committed",
        "base_version": 1, "new_version": 2, "reasons": []
    })
}

fn approval_requested(approval: &str) -> Value {
    json!({
        "type": "approval_requested", "approval_id": approval, "gate": "approval",
        "subject_id": "tr-1", "summary": "Commit the final design."
    })
}

/// A completion event appended again, even as a recovered copy, returns the
/// one stored and takes no sequence number; other events are all stored.
async fn completion_events_are_stored_once<S: EventLog>(f: Fenced<S>) {
    let first = f.append(run_started()).await;
    assert_eq!(
        f.append(run_started()).await,
        first,
        "completions: run_started again"
    );
    let completed = f.append(tool_call_completed("step-1.1", 40, None)).await;
    assert_eq!(
        f.append(tool_call_completed("step-1.1", 0, Some(true)))
            .await,
        completed,
        "completions: a recovered tool_call_completed returns the first"
    );
    let other_step = f.append(tool_call_completed("step-1.2", 40, None)).await;
    assert_ne!(other_step.seq, completed.seq, "completions: another step");
    let transition = f.append(sdr_transition("tr-1")).await;
    assert_eq!(
        f.append(sdr_transition("tr-1")).await,
        transition,
        "completions: sdr_transition again"
    );
    let asked = f.append(approval_requested("appr-1")).await;
    assert_eq!(
        f.append(approval_requested("appr-1")).await,
        asked,
        "completions: approval_requested again"
    );
    append_marker(f.log.as_ref(), "run-1", 7).await;
    append_marker(f.log.as_ref(), "run-1", 7).await;

    let all = f.log.read(&run("run-1"), 0, 100).await.expect("read");
    assert_eq!(
        seqs(&all),
        (1..=7).collect::<Vec<u64>>(),
        "completions: one of each completion, both markers, no gap"
    );
    assert_eq!(
        f.log.last_seq(&run("run-1")).await.expect("last_seq"),
        7,
        "completions: last_seq"
    );
}

async fn concurrent_completions_are_stored_once<S: EventLog + 'static>(f: Fenced<S>) {
    const WRITERS: usize = 16;
    let f = Arc::new(f);
    let mut handles = Vec::new();
    for marker in 0..WRITERS {
        let f = Arc::clone(&f);
        handles.push(tokio::spawn(async move {
            let completed = f
                .append(tool_call_completed("step-1.1", marker as u64, None))
                .await;
            append_marker(f.log.as_ref(), "run-1", marker).await;
            completed.seq.get()
        }));
    }
    let mut completions = HashSet::new();
    for handle in handles {
        completions.insert(handle.await.expect("concurrent completions: task panicked"));
    }
    assert_eq!(
        completions.len(),
        1,
        "concurrent completions: every writer got the same stored event"
    );
    let all = f.log.read(&run("run-1"), 0, 1000).await.expect("read");
    assert_eq!(
        seqs(&all),
        (1..=WRITERS as u64 + 1).collect::<Vec<u64>>(),
        "concurrent completions: one completion and every marker, no gap"
    );
}

async fn reading_by_kind_returns_only_those_kinds<S: EventLog>(f: Fenced<S>) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let answered =
        json!({ "type": "approval_answered", "approval_id": "appr-1", "answer": "approve" });
    f.append(run_started()).await; // 1
    append_marker(f.log.as_ref(), "run-1", 0).await; // 2
    f.append(tool_call_completed("step-1.1", 5, None)).await; // 3
    f.log
        .append(
            new_event("run-1", at, answered),
            Some(&unfenced(UnfencedReason::ApiCommand)),
        )
        .await
        .expect("append an answer"); // 4
    append_marker(f.log.as_ref(), "run-1", 1).await; // 5
    append_marker(f.log.as_ref(), "run-other", 2).await;

    const WAKE: &[EventKind] = &[EventKind::APPROVAL_ANSWERED, EventKind::CANCEL_REQUESTED];
    let read = |after, kinds: &'static [EventKind], limit| {
        let log = Arc::clone(&f.log);
        async move {
            log.read_kinds(&run("run-1"), after, kinds, limit)
                .await
                .expect("read_kinds")
        }
    };
    assert_eq!(seqs(&read(0, WAKE, 100).await), [2, 4, 5], "by kind: all");
    assert_eq!(seqs(&read(2, WAKE, 100).await), [4, 5], "by kind: after 2");
    assert_eq!(seqs(&read(0, WAKE, 2).await), [2, 4], "by kind: limit");
    assert_eq!(
        seqs(&read(0, &[EventKind::RUN_STARTED], 100).await),
        [1],
        "by kind: one kind"
    );
    assert!(read(0, &[], 100).await.is_empty(), "by kind: no kinds");
    assert!(
        read(0, &[EventKind::SDR_TRANSITION], 100).await.is_empty(),
        "by kind: a kind the run has none of"
    );
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
