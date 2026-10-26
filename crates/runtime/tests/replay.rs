//! Exact replay (ADR-0035): every model call of a run is recorded before its
//! `model_call_completed`, and the record rebuilds the request the provider
//! received and the context the log hashed.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::collections::BTreeSet;

use contracts::ReplayStore;
use contracts::ids::RunId;
use contracts::replay::{fetch, rebuild};
use contracts::types::{EventPayload, StopReason};
use support::*;
use testkit::builders::id;

#[tokio::test]
async fn every_call_of_a_run_replays_exactly() {
    let world = World::new(Setup::new(vec![
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
    ]))
    .await;
    let ran = world
        .run(job_for(task_json(&["R-mass"], roomy_budget())))
        .await;
    assert_eq!(ran.outcome().stop_reason, StopReason::Success);

    let events = world.logged().await;
    let contexts: Vec<(u64, _)> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ContextAssembled { context_hash, .. } => {
                Some((e.seq.get(), context_hash))
            }
            _ => None,
        })
        .collect();
    let completed = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::ModelCallCompleted { .. }))
        .count();
    let sent = world.model.requests();
    assert_eq!(contexts.len(), 3);
    assert_eq!(completed, 3);
    assert_eq!(sent.len(), 3);

    let run_id: RunId = id(RUN);
    let records = world.replay.calls_for_run(&run_id).await.expect("records");
    assert_eq!(records.len(), 3, "one record per completed call");
    for ((seq, logged_hash), (record, provider_saw)) in
        contexts.iter().zip(records.iter().zip(&sent))
    {
        assert_eq!(record.context_seq.get(), *seq, "keyed by context_assembled");
        assert_eq!(&record.context_hash, *logged_hash);
        let blobs = fetch(world.replay.as_ref(), record).await.expect("blobs");
        let (request, response) = rebuild(record, &blobs).expect("rebuilds");
        assert_eq!(request.system, provider_saw.system);
        assert_eq!(request.messages, provider_saw.messages);
        assert_eq!(request.tools, provider_saw.tools);
        assert_eq!(request.max_output_tokens, provider_saw.max_output_tokens);
        let rehashed = context::context_hash(&request.system, &request.messages).expect("hash");
        assert_eq!(&rehashed, *logged_hash, "the replay is the logged context");
        assert!(!response.content.is_empty());
    }

    // The instructions and tool definitions are stored once for the run.
    let systems: BTreeSet<&str> = records.iter().map(|r| r.system.as_str()).collect();
    let tools: BTreeSet<&str> = records.iter().map(|r| r.tools.as_str()).collect();
    assert_eq!((systems.len(), tools.len()), (1, 1));
}
