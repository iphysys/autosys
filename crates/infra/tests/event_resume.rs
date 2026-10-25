//! Resuming the event stream while appends continue: PR 4's third done-when
//! test. Needs `DATABASE_URL` (see README, section Database).

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use contracts::ids::RunId;
use contracts::types::{Event, EventPayload};
use contracts::{EventLog, ExecutionContext};
use infra::persistence::PgEventLog;
use support::TestDb;
use testkit::builders::{id, marker_payload, new_event};
use testkit::leases::api_command;

const WRITERS: usize = 4;
const PER_WRITER: usize = 50;
const TOTAL: usize = WRITERS * PER_WRITER;
/// Small pages, so readers resume from a sequence number many times.
const PAGE: usize = 7;
const WAIT: Duration = Duration::from_secs(10);

/// Reader A follows a run while four writers append to it, and disconnects
/// halfway. Reader B, on a new connection, resumes from A's last sequence
/// number. Each reader must see consecutive sequence numbers, and together
/// they must see every event exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_resumed_reader_sees_every_later_event_exactly_once() {
    let db = TestDb::create().await;
    let writers_log = Arc::new(
        PgEventLog::connect(db.database())
            .await
            .expect("cannot start the event listener"),
    );
    let run: RunId = id("run-1");

    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let log = Arc::clone(&writers_log);
        writers.push(tokio::spawn(async move {
            for n in 0..PER_WRITER {
                let marker = w * PER_WRITER + n;
                // The markers are cancel_requested events, a client command.
                log.append(
                    new_event("run-1", Utc::now(), marker_payload(marker)),
                    Some(&api_command()),
                )
                .await
                .expect("append");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }));
    }

    let reader_a = PgEventLog::connect(db.database())
        .await
        .expect("cannot start the event listener");
    let first = follow(&reader_a, &run, 0, TOTAL / 2).await;
    drop(reader_a);
    let resume_after = first.last().map_or(0, |e| e.seq.get());

    let reader_b = PgEventLog::connect(db.database())
        .await
        .expect("cannot start the event listener");
    let second = follow(&reader_b, &run, resume_after, TOTAL - first.len()).await;

    for writer in writers {
        writer.await.expect("a writer panicked");
    }
    let seen: Vec<u64> = first.iter().chain(&second).map(|e| e.seq.get()).collect();
    let expected: Vec<u64> = (1..=TOTAL as u64).collect();
    assert_eq!(
        seen, expected,
        "events were missed or repeated across the resume"
    );
    let markers: HashSet<String> = first.iter().chain(&second).map(marker_of).collect();
    assert_eq!(markers.len(), TOTAL, "an event's content was repeated");

    let more = reader_b.read(&run, TOTAL as u64, 100).await.expect("read");
    assert!(more.is_empty(), "events past the last append");
    db.drop().await;
}

/// Reads `want` events after `after`, waiting for appends as needed. Fails if
/// a sequence number is skipped or repeated.
async fn follow(log: &PgEventLog, run: &RunId, mut after: u64, want: usize) -> Vec<Event> {
    let ctx = ExecutionContext::new(id("corr-follow"));
    let mut seen = Vec::new();
    while seen.len() < want {
        tokio::time::timeout(WAIT, log.wait_for(run, after, &ctx))
            .await
            .expect("no new event arrived")
            .expect("wait_for");
        for event in log.read(run, after, PAGE).await.expect("read") {
            assert_eq!(
                event.seq.get(),
                after + 1,
                "a sequence number was skipped or repeated"
            );
            after = event.seq.get();
            seen.push(event);
            if seen.len() == want {
                break;
            }
        }
    }
    seen
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
