//! What must hold after any run, however many workers crashed during it
//! (blueprint section 6.3, ADR-0031): the log, the job and the SDR agree,
//! and nothing happened twice.

use std::collections::{BTreeMap, BTreeSet};

use bootstrap::Worker;
use contracts::types::{Event, EventPayload, Job, JobState, JobStatus};
use contracts::{SchemaSet, job_state};
use serde_json::Value;
use testkit::fakes::KeyedEffects;

use super::{head, logged};

const EVENT_SCHEMA: &str = "events/event.schema.json";

/// The payload's `type` and its JSON.
fn payload(event: &Event) -> (String, Value) {
    let value = serde_json::to_value(&event.payload).expect("a payload serializes");
    let kind = value["type"].as_str().unwrap_or_default().to_owned();
    (kind, value)
}

/// How many times each value of `field` appears in events of `kind`.
fn counts(events: &[Event], kind: &str, field: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        let (k, value) = payload(event);
        if k == kind {
            let key = value[field].as_str().unwrap_or_default().to_owned();
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

fn assert_once(events: &[Event], kind: &str, field: &str) {
    let repeated: Vec<(String, usize)> = counts(events, kind, field)
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .collect();
    assert!(
        repeated.is_empty(),
        "{kind} repeated per {field}: {repeated:?}"
    );
}

/// Checks every invariant for the job of `run`, as `worker` reads it.
/// `effects` holds the keyed tool's store, if the test used one.
pub async fn hold(worker: &Worker, job: &Job, effects: Option<&KeyedEffects>) {
    let run = job.run_id.as_str();
    let events = logged(worker, run).await;

    // The sequence is 1..N, without gaps.
    let seqs: Vec<u64> = events.iter().map(|e| e.seq.get()).collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "the log of {run} has gaps");

    // Every event is valid against the event schema.
    let schemas = SchemaSet::compile(&[EVENT_SCHEMA]).expect("the event schema compiles");
    for event in &events {
        let problems = schemas
            .errors_of(EVENT_SCHEMA, event)
            .expect("the schema runs");
        assert!(
            problems.is_empty(),
            "event {} is invalid: {problems:?}",
            event.seq.get()
        );
    }

    // Completion events happen once.
    let started = events
        .iter()
        .filter(|e| payload(e).0 == "run_started")
        .count();
    assert_eq!(started, 1, "run_started of {run}");
    assert_once(&events, "tool_call_completed", "step_id");
    assert_once(&events, "approval_requested", "approval_id");
    assert_once(&events, "sdr_transition", "transition_id");

    // Every committed transition made one version, and the head is the last.
    let committed: Vec<u64> = events
        .iter()
        .map(payload)
        .filter(|(k, v)| k == "sdr_transition" && v["outcome"] == "committed")
        .filter_map(|(_, v)| v["new_version"].as_u64())
        .collect();
    let distinct: BTreeSet<u64> = committed.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        committed.len(),
        "two commits made one version"
    );
    if let Some(last) = committed.iter().max() {
        assert_eq!(head(worker).await, *last, "the head is not the last commit");
    }

    // One effect per idempotency key.
    if let Some(effects) = effects {
        let keys: BTreeSet<String> = events
            .iter()
            .map(payload)
            .filter(|(k, _)| k == "tool_call_started")
            .filter_map(|(_, v)| v["idempotency_key"].as_str().map(str::to_owned))
            .collect();
        for key in &keys {
            assert!(
                effects.effect(key).is_none() || effects.calls(key) >= 1,
                "an effect of {key} without a call"
            );
        }
        assert!(
            effects.effects() <= keys.len(),
            "{} effects for {} started calls",
            effects.effects(),
            keys.len()
        );
    }

    // The job_state_changed records form one chain that ends in the job's
    // state, and the last one is the job's status.
    let chain: Vec<(Option<JobState>, JobStatus)> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::JobStateChanged { from, to, .. } => Some((*from, to.clone())),
            _ => None,
        })
        .collect();
    let mut at: Option<JobState> = None;
    for (from, to) in &chain {
        assert_eq!(*from, at, "the chain of {run} breaks: {chain:?}");
        at = Some(job_state(to));
    }
    let stored = worker.jobs.get(&job.id).await.expect("the job");
    assert_eq!(
        at,
        Some(job_state(&stored.status)),
        "the chain ends elsewhere"
    );
    let last = chain.last().map(|(_, to)| to.clone());
    assert_eq!(
        last.as_ref(),
        Some(&stored.status),
        "the last record is not the status"
    );

    // A status that names a checkpoint names the record the job holds.
    let named = match &stored.status {
        JobStatus::Waiting { checkpoint, .. }
        | JobStatus::Suspended { checkpoint, .. }
        | JobStatus::Completed { checkpoint, .. }
        | JobStatus::Cancelled { checkpoint }
        | JobStatus::Failed { checkpoint, .. } => Some(checkpoint.clone()),
        JobStatus::Queued { checkpoint } => checkpoint.clone(),
        JobStatus::Running => None,
    };
    if let Some(named) = named {
        let record = worker
            .jobs
            .continuation(&job.id)
            .await
            .expect("read the record")
            .expect("a status that names a checkpoint has a record");
        assert_eq!(
            runtime::checkpoint_ref(&record).expect("a reference"),
            named,
            "the status names a checkpoint the job does not hold"
        );
    }

    // A finished job has one terminal run_stopped, and its stop reason.
    let terminal: Vec<String> = events
        .iter()
        .map(payload)
        .filter(|(k, v)| k == "run_stopped" && v["stop_reason"] != "approval_required")
        .map(|(_, v)| v["stop_reason"].as_str().unwrap_or_default().to_owned())
        .collect();
    match &stored.status {
        JobStatus::Completed { stop_reason, .. } => {
            assert_eq!(
                terminal,
                [stop_reason.to_string()],
                "terminal stops of {run}"
            );
        }
        JobStatus::Cancelled { .. } => assert_eq!(terminal, ["cancelled"]),
        _ => assert!(terminal.len() <= 1, "terminal stops of {run}: {terminal:?}"),
    }
}
