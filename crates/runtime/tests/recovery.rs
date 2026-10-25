//! A run saves its state at every phase boundary and continues from the
//! last checkpoint alone (ADR-0031). Here a "crash" drops the run while it
//! saves a chosen checkpoint, so that checkpoint is lost and the stores keep
//! whatever the run wrote before it; the run then continues from the
//! checkpoint before, in the same world.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::Error;
use contracts::types::{ApprovalAnswer, ContinuationRecord, EventPayload, Job, StopReason};
use runtime::{CheckpointSink, Start};
use support::*;
use tokio::sync::Notify;

/// Keeps every checkpoint, except that the one numbered `crash_at` never
/// finishes saving: the test drops the run there.
#[derive(Default)]
struct Checkpoints {
    saved: Mutex<Vec<(Job, ContinuationRecord)>>,
    crash_at: Option<u64>,
    crashed: Notify,
}

impl Checkpoints {
    fn crashing_at(checkpoint: u64) -> Self {
        Self {
            crash_at: Some(checkpoint),
            ..Self::default()
        }
    }

    fn last(&self) -> (Job, ContinuationRecord) {
        self.saved
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a checkpoint was saved")
    }

    fn numbers(&self) -> Vec<u64> {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.checkpoint.get())
            .collect()
    }
}

#[async_trait]
impl CheckpointSink for Checkpoints {
    async fn checkpoint(&self, job: &Job, record: &ContinuationRecord) -> Result<(), Error> {
        if Some(record.checkpoint.get()) == self.crash_at {
            self.crashed.notify_one();
            std::future::pending::<()>().await;
        }
        self.saved
            .lock()
            .unwrap()
            .push((job.clone(), record.clone()));
        Ok(())
    }
}

/// Runs from `start`; `None` if the run crashed at the sink's checkpoint.
async fn drive(world: &World, start: Start, sink: &Checkpoints) -> Option<Ran> {
    let stream = world.orchestrator.drive(start, sink, world.ctx());
    tokio::select! {
        ran = collect(stream) => Some(ran),
        () = sink.crashed.notified() => None,
    }
}

fn job() -> Job {
    job_for(task_json(&["R-mass"], roomy_budget()))
}

fn measured() -> Vec<ScriptStep> {
    vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        call_tools(vec![("c2", "measure", measure_args())]),
    ]
}

fn propose_measured_mass(is_final: bool) -> ScriptStep {
    from_request(move |request| {
        let evidence = last_evidence(request);
        tool_use(&[(
            "c3".to_owned(),
            "sdr_propose".to_owned(),
            propose_mass(MASS_G, &evidence, is_final),
        )])
    })
}

/// The log of run-1 has seq 1..n, one run_started, one tool_call_completed
/// per step and one sdr_transition per transition.
async fn assert_log_is_whole(world: &World) {
    let logged = world.logged().await;
    let seqs: Vec<u64> = logged.iter().map(|e| e.seq.get()).collect();
    assert_eq!(seqs, (1..=logged.len() as u64).collect::<Vec<_>>(), "gaps");
    let mut completions: HashMap<String, usize> = HashMap::new();
    for event in &logged {
        let key = match &event.payload {
            EventPayload::RunStarted { .. } => "run_started".to_owned(),
            EventPayload::ToolCallCompleted { step_id, .. } => format!("tool:{}", step_id.as_str()),
            EventPayload::SdrTransition { transition_id, .. } => {
                format!("sdr:{}", transition_id.as_str())
            }
            _ => continue,
        };
        *completions.entry(key).or_default() += 1;
    }
    for (key, count) in completions {
        assert_eq!(count, 1, "{key} was written {count} times");
    }
}

#[tokio::test]
async fn checkpoints_are_numbered_and_the_last_record_is_returned() {
    let mut script = measured();
    script.push(propose_measured_mass(true));
    let world = World::new(Setup::new(script)).await;
    let sink = Checkpoints::default();
    let ran = drive(&world, Start::New(Box::new(job())), &sink)
        .await
        .expect("no crash");
    let outcome = ran.outcome();
    assert_eq!(outcome.stop_reason, StopReason::Success);
    // A reply and a result for each tool step, then the proposal's reply;
    // the final commit stops the run.
    assert_eq!(sink.numbers(), [1, 2, 3, 4, 5]);
    assert_eq!(outcome.record.checkpoint.get(), 6);
    assert_eq!(outcome.record.committed.len(), 1);
    let (saved_job, first) = sink.saved.lock().unwrap()[0].clone();
    let step = first.open_step.expect("the first reply's step");
    assert_eq!(step.next, 0);
    assert!(first.approval_id.is_none());
    assert!(
        saved_job.progress.last_event_seq > 0,
        "a checkpoint holds the events forwarded before it"
    );
}

/// The checkpoint after create_plate's result is lost: the next run
/// continues at the first reply, finds create_plate's step record and does
/// not call it again.
#[tokio::test]
async fn a_lost_checkpoint_repeats_no_tool_call() {
    let mut script = measured();
    script.push(propose_measured_mass(true));
    let world = World::new(Setup::new(script)).await;
    let crashing = Checkpoints::crashing_at(2);
    assert!(
        drive(&world, Start::New(Box::new(job())), &crashing)
            .await
            .is_none(),
        "the run crashed"
    );
    assert_eq!(world.create_plate.calls(), 1);

    let (saved_job, record) = crashing.last();
    assert_eq!(record.checkpoint.get(), 1);
    let sink = Checkpoints::default();
    let ran = drive(
        &world,
        Start::Recover {
            job: Box::new(saved_job),
            record: Some(Box::new(record)),
        },
        &sink,
    )
    .await
    .expect("no crash");
    assert_eq!(ran.outcome().stop_reason, StopReason::Success);
    assert_eq!(
        world.create_plate.calls(),
        1,
        "create_plate was called again"
    );
    assert_eq!(world.measure.calls(), 1);
    assert_eq!(sink.numbers().first(), Some(&2), "numbering goes on");
    assert_eq!(world.head().await, 2);
    assert_log_is_whole(&world).await;
}

/// A proposal commits, and the checkpoint after it is lost. The next run
/// proposes again from the reply, finds the commit by its transition id,
/// and goes on without a second commit.
#[tokio::test]
async fn a_commit_before_a_lost_checkpoint_is_not_made_again() {
    let mut script = measured();
    script.push(propose_measured_mass(false));
    script.push(from_request(|_| {
        tool_use(&[(
            "c4".to_owned(),
            "sdr_propose".to_owned(),
            propose_decision(true),
        )])
    }));
    let world = World::new(Setup::new(script)).await;
    // 1-4: the two tool steps; 5: the proposal's reply; 6: after its commit.
    let crashing = Checkpoints::crashing_at(6);
    assert!(
        drive(&world, Start::New(Box::new(job())), &crashing)
            .await
            .is_none(),
        "the run crashed"
    );
    assert_eq!(world.head().await, 2, "the commit landed");

    let (saved_job, record) = crashing.last();
    assert_eq!(record.checkpoint.get(), 5);
    let ran = drive(
        &world,
        Start::Recover {
            job: Box::new(saved_job),
            record: Some(Box::new(record)),
        },
        &Checkpoints::default(),
    )
    .await
    .expect("no crash");
    let outcome = ran.outcome();
    assert_eq!(outcome.result.committed_transition_ids.len(), 2);
    assert_eq!(world.head().await, 3, "a commit was made twice");
    assert_log_is_whole(&world).await;
}

/// A run that stopped for an approval continues from its returned record
/// alone, with the answer.
#[tokio::test]
async fn an_approval_continues_from_the_stored_record() {
    let mut script = measured();
    script.push(propose_measured_mass(true));
    let world = World::new(Setup::new(script).autonomy(committed_autonomy())).await;
    let sink = Checkpoints::default();
    let waiting = drive(&world, Start::New(Box::new(job())), &sink)
        .await
        .expect("no crash");
    let outcome = waiting.outcome();
    assert_eq!(outcome.stop_reason, StopReason::ApprovalRequired);
    assert!(outcome.record.approval_id.is_some());
    assert!(outcome.record.open_step.is_some());
    assert_eq!(world.head().await, 1);

    let answered = drive(
        &world,
        Start::Answer {
            job: Box::new(outcome.job.clone()),
            record: Box::new(outcome.record.clone()),
            answer: ApprovalAnswer::Approve,
        },
        &Checkpoints::default(),
    )
    .await
    .expect("no crash");
    assert_eq!(answered.outcome().stop_reason, StopReason::Success);
    assert_eq!(world.head().await, 2);
    assert_eq!(world.create_plate.calls(), 1);
    assert_log_is_whole(&world).await;
}
