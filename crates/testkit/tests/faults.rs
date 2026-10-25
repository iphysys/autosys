//! The doubles that stop a worker, the manual spawner and keyed effects:
//! what the crash tests of PR 12 rely on.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use contracts::{BackgroundTask, EventKind, EventLog, JobStore, Spawner, Tool};
use serde_json::json;
use testkit::builders::{id, job, marker_payload, new_event, tool_request};
use testkit::clock::ManualClock;
use testkit::fakes::{KeyedEffects, go_on};
use testkit::faults::{At, EventFaults, JobFaults, Pause};
use testkit::leases::{LONG_LEASE, api_command, job_submit};
use testkit::spawn::ManualSpawner;
use testkit::stores::{InMemoryEventLog, InMemoryJobStore};

const WAIT: Duration = Duration::from_secs(5);

fn paused_log(at: At) -> (Arc<dyn EventLog>, Arc<dyn EventLog>, Pause) {
    let inner: Arc<dyn EventLog> = Arc::new(InMemoryEventLog::new());
    let pause = Pause::new("cancel", at);
    let wrapped =
        EventFaults::pause_kind(&pause, EventKind::CANCEL_REQUESTED).wrapper()(Arc::clone(&inner));
    (inner, wrapped, pause)
}

#[tokio::test]
async fn a_pause_before_holds_the_write_back_until_released() {
    let (inner, log, pause) = paused_log(At::Before);
    let writer = {
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            log.append(
                new_event("run-1", Utc::now(), marker_payload(1)),
                Some(&api_command()),
            )
            .await
        })
    };
    pause.reached(WAIT).await;
    assert_eq!(inner.last_seq(&id("run-1")).await.unwrap(), 0, "written");
    pause.release();
    writer.await.unwrap().expect("the append goes on");
    assert_eq!(inner.last_seq(&id("run-1")).await.unwrap(), 1);

    // Only the first match stops.
    log.append(
        new_event("run-1", Utc::now(), marker_payload(2)),
        Some(&api_command()),
    )
    .await
    .expect("a second append does not stop");
}

#[tokio::test]
async fn a_pause_after_stops_once_the_write_is_done() {
    let (inner, log, pause) = paused_log(At::After);
    let writer = {
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            log.append(
                new_event("run-1", Utc::now(), marker_payload(1)),
                Some(&api_command()),
            )
            .await
        })
    };
    pause.reached(WAIT).await;
    assert_eq!(
        inner.last_seq(&id("run-1")).await.unwrap(),
        1,
        "not written"
    );
    // A worker killed here never returns; the write stays.
    writer.abort();
    assert_eq!(inner.last_seq(&id("run-1")).await.unwrap(), 1);
}

#[tokio::test]
async fn job_faults_record_every_claim() {
    let clock = Arc::new(ManualClock::at_epoch());
    let inner: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new(clock.clone()));
    let faults = JobFaults::new();
    let jobs = faults.wrapper()(inner);
    jobs.insert(&job("job-1", "run-1", Utc::now()), Some(&job_submit()))
        .await
        .unwrap();
    let claimed = jobs
        .claim(&id("worker-1"), LONG_LEASE)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(faults.claims(), [claimed]);
}

fn counting_task(runs: &Arc<AtomicUsize>) -> BackgroundTask {
    let runs = Arc::clone(runs);
    Box::pin(async move {
        runs.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
    })
}

#[tokio::test]
async fn a_holding_spawner_starts_a_task_only_when_released() {
    let spawner = ManualSpawner::holding();
    let runs = Arc::new(AtomicUsize::new(0));
    let handle = spawner.spawn("heartbeat", counting_task(&runs));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a held task ran");
    assert_eq!(spawner.held(), ["heartbeat"]);
    assert_eq!(spawner.release("heartbeat"), 1);
    tokio::time::timeout(WAIT, async {
        while runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the released task runs");
    assert_eq!(spawner.started(), ["heartbeat"]);
    drop(handle);
    assert!(spawner.started().is_empty(), "the dropped task still runs");

    // A held task dropped before release never runs.
    let never = Arc::new(AtomicUsize::new(0));
    drop(spawner.spawn("heartbeat", counting_task(&never)));
    assert!(spawner.held().is_empty());
}

#[tokio::test]
async fn keyed_effects_keep_one_effect_per_key_across_tool_instances() {
    let effects = KeyedEffects::new();
    let made = Arc::new(AtomicUsize::new(0));
    let tool = |effects: &KeyedEffects| {
        let made = Arc::clone(&made);
        effects.tool(
            "create_plate",
            move |request| {
                let n = made.fetch_add(1, Ordering::SeqCst);
                json!({ "part_id": format!("part-{n}"), "arguments": request.arguments })
            },
            go_on,
        )
    };
    let ctx = contracts::ExecutionContext::new(id("corr"));
    let request = tool_request("create_plate", "step-1.1", json!({ "length_mm": 80 }));
    let first = tool(&effects).invoke(&request, &ctx).await.unwrap();
    // A worker started after a crash calls again with the same key.
    let again = tool(&effects).invoke(&request, &ctx).await.unwrap();
    assert_eq!(first, again, "the stored effect is answered");
    assert_eq!(made.load(Ordering::SeqCst), 1, "a second effect was made");
    assert_eq!(effects.effects(), 1);
    assert_eq!(effects.calls(request.idempotency_key.as_str()), 2);

    let other = tool_request("create_plate", "step-1.2", json!({ "length_mm": 80 }));
    tool(&effects).invoke(&other, &ctx).await.unwrap();
    assert_eq!(effects.effects(), 2, "another key, another effect");
}

#[tokio::test]
async fn a_tool_hook_stops_at_a_pause_once() {
    let pause = Pause::new("tool", At::Before);
    let hook = {
        let pause = pause.clone();
        move |_: usize, _: &contracts::ToolRequest| {
            let pause = pause.clone();
            Box::pin(async move {
                pause.stop().await;
                Ok(())
            }) as testkit::fakes::HookFuture
        }
    };
    let tool: Arc<dyn Tool> = Arc::new(KeyedEffects::new().tool("t", |_| json!({}), hook));
    let ctx = contracts::ExecutionContext::new(id("corr"));
    let first = {
        let (tool, ctx) = (Arc::clone(&tool), ctx.clone());
        tokio::spawn(async move {
            let request = tool_request("t", "step-1.1", json!({}));
            tool.invoke(&request, &ctx).await
        })
    };
    pause.reached(WAIT).await;
    assert!(!first.is_finished(), "the first call waits at the pause");
    let later = tool_request("t", "step-1.2", json!({}));
    tool.invoke(&later, &ctx)
        .await
        .expect("a later call does not stop");
    pause.release();
    first
        .await
        .unwrap()
        .expect("the first call goes on once released");
}
