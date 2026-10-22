use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use contracts::ids::{Identifier, JobId};
use contracts::types::{JobState, StopReason, WakeCondition};
use contracts::{Clock, ErrorCode, JobStore, WakeReason};

use super::assert_code;
use crate::builders::{id, job};
use crate::clock::ManualClock;

const LEASE: Duration = Duration::from_secs(30);

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

/// The `JobStore` contract. `make` builds an empty store that reads time from
/// the given clock.
pub async fn job_store<S, F, Fut>(make: F)
where
    S: JobStore + 'static,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = S>,
{
    insert_then_get(fresh(&make).await).await;
    duplicate_insert_is_conflict(fresh(&make).await).await;
    insert_requires_a_queued_job(fresh(&make).await).await;
    missing_job_is_not_found(fresh(&make).await).await;
    find_by_run(fresh(&make).await).await;
    claim_takes_the_oldest_job(fresh(&make).await).await;
    concurrent_claimers_never_share_a_job(fresh(&make).await).await;
    lease_renewal(fresh(&make).await).await;
    save_requires_the_lease(fresh(&make).await).await;
    leaving_running_clears_the_lease(fresh(&make).await).await;
    expired_lease_can_be_reclaimed(fresh(&make).await).await;
    wake_follows_the_wake_condition(fresh(&make).await).await;
}

fn worker(n: usize) -> Identifier {
    id(&format!("worker-{n}"))
}

async fn insert_then_get<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store.insert(&new).await.expect("insert_then_get: insert");
    let stored = f.store.get(&new.id).await.expect("insert_then_get: get");
    assert_eq!(stored, new, "insert_then_get: stored job differs");
}

async fn duplicate_insert_is_conflict<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store
        .insert(&new)
        .await
        .expect("duplicate_insert: first insert");
    assert_code(
        f.store.insert(&new).await,
        ErrorCode::Conflict,
        "duplicate_insert_is_conflict",
    );
}

async fn insert_requires_a_queued_job<S: JobStore>(f: Fixture<S>) {
    let mut running = job("job-1", "run-1", f.clock.now());
    running.state = JobState::Running;
    assert_code(
        f.store.insert(&running).await,
        ErrorCode::InvalidInput,
        "insert_requires_a_queued_job",
    );
}

async fn missing_job_is_not_found<S: JobStore>(f: Fixture<S>) {
    let missing: JobId = id("job-missing");
    assert_code(
        f.store.get(&missing).await,
        ErrorCode::NotFound,
        "missing_job_is_not_found: get",
    );
    assert_code(
        f.store.wake(&missing, &WakeReason::CancelRequested).await,
        ErrorCode::NotFound,
        "missing_job_is_not_found: wake",
    );
}

async fn find_by_run<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store.insert(&new).await.expect("find_by_run: insert");
    let found = f
        .store
        .find_by_run(&id("run-1"))
        .await
        .expect("find_by_run");
    assert_eq!(found, Some(new), "find_by_run: wrong job");
    let none = f
        .store
        .find_by_run(&id("run-2"))
        .await
        .expect("find_by_run");
    assert_eq!(none, None, "find_by_run: found a job for an unknown run");
}

async fn claim_takes_the_oldest_job<S: JobStore>(f: Fixture<S>) {
    let now = f.clock.now();
    let newer = job("job-a", "run-a", now + TimeDelta::seconds(1));
    let older = job("job-b", "run-b", now);
    f.store.insert(&newer).await.expect("claim_oldest: insert");
    f.store.insert(&older).await.expect("claim_oldest: insert");

    let first = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("claim_oldest: claim")
        .expect("claim_oldest: a queued job was not claimed");
    assert_eq!(first.id, older.id, "claim_oldest: not the oldest job");
    assert_eq!(first.state, JobState::Running, "claim_oldest: state");
    assert_eq!(first.attempt, 1, "claim_oldest: attempt");
    let lease = first.lease.clone().expect("claim_oldest: no lease");
    assert_eq!(lease.holder, worker(1), "claim_oldest: lease holder");
    assert_eq!(
        lease.expires_at.0,
        now + TimeDelta::seconds(30),
        "claim_oldest: lease expiry"
    );
    let stored = f.store.get(&older.id).await.expect("claim_oldest: get");
    assert_eq!(stored, first, "claim_oldest: claim was not persisted");

    let second = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("claim_oldest: claim");
    assert_eq!(
        second.map(|j| j.id),
        Some(newer.id),
        "claim_oldest: second claim"
    );
    let third = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("claim_oldest: claim");
    assert_eq!(
        third, None,
        "claim_oldest: claimed a running job with a live lease"
    );
}

async fn concurrent_claimers_never_share_a_job<S: JobStore + 'static>(f: Fixture<S>) {
    const JOBS: usize = 8;
    const CLAIMERS: usize = 16;
    let now = f.clock.now();
    for n in 0..JOBS {
        let new = job(&format!("job-{n}"), &format!("run-{n}"), now);
        f.store
            .insert(&new)
            .await
            .expect("concurrent_claimers: insert");
    }
    let mut handles = Vec::new();
    for n in 0..CLAIMERS {
        let store = Arc::clone(&f.store);
        handles.push(tokio::spawn(async move {
            let holder = worker(n);
            let mut claimed = Vec::new();
            while let Some(job) = store
                .claim(&holder, LEASE)
                .await
                .expect("concurrent_claimers: claim")
            {
                claimed.push(job.id.as_str().to_owned());
            }
            claimed
        }));
    }
    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.await.expect("concurrent_claimers: task panicked"));
    }
    let unique: HashSet<&String> = all.iter().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "concurrent_claimers: a job was claimed twice: {all:?}"
    );
    assert_eq!(
        all.len(),
        JOBS,
        "concurrent_claimers: not every job was claimed"
    );
}

async fn lease_renewal<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store.insert(&new).await.expect("lease_renewal: insert");
    f.store
        .claim(&worker(1), LEASE)
        .await
        .expect("lease_renewal: claim");
    f.clock.advance(Duration::from_secs(10));
    let renewed = f
        .store
        .renew_lease(&new.id, &worker(1), LEASE)
        .await
        .expect("lease_renewal: holder renews");
    let lease = renewed
        .lease
        .expect("lease_renewal: no lease after renewal");
    let now = f.clock.now();
    assert_eq!(
        lease.expires_at.0,
        now + TimeDelta::seconds(30),
        "lease_renewal: expiry"
    );
    assert_eq!(lease.heartbeat_at.0, now, "lease_renewal: heartbeat");
    assert_code(
        f.store.renew_lease(&new.id, &worker(2), LEASE).await,
        ErrorCode::Conflict,
        "lease_renewal: renewal by a non-holder",
    );
}

async fn save_requires_the_lease<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store
        .insert(&new)
        .await
        .expect("save_requires_lease: insert");
    let mut claimed = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("save_requires_lease: claim")
        .expect("save_requires_lease: no job");
    claimed.progress.steps_completed = 3;
    assert_code(
        f.store.save(&claimed, &worker(2)).await,
        ErrorCode::Conflict,
        "save_requires_the_lease: save by a non-holder",
    );
    f.store
        .save(&claimed, &worker(1))
        .await
        .expect("save_requires_lease: save by the holder");
    let stored = f
        .store
        .get(&new.id)
        .await
        .expect("save_requires_lease: get");
    assert_eq!(
        stored.progress.steps_completed, 3,
        "save_requires_lease: progress lost"
    );

    // The store owns the lease and attempt count; a caller cannot rewrite them.
    let mut forged = stored.clone();
    forged.attempt = 99;
    forged.lease = None;
    f.store
        .save(&forged, &worker(1))
        .await
        .expect("save_requires_lease: save");
    let stored = f
        .store
        .get(&new.id)
        .await
        .expect("save_requires_lease: get");
    assert_eq!(
        stored.attempt, 1,
        "save_requires_lease: attempt was overwritten"
    );
    assert_eq!(
        stored.lease.map(|l| l.holder),
        Some(worker(1)),
        "save_requires_lease: lease was overwritten"
    );
}

async fn leaving_running_clears_the_lease<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store.insert(&new).await.expect("leaving_running: insert");
    let mut claimed = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("leaving_running: claim")
        .expect("leaving_running: no job");
    claimed.state = JobState::Completed;
    claimed.stop_reason = Some(StopReason::Success);
    f.store
        .save(&claimed, &worker(1))
        .await
        .expect("leaving_running: save");
    let stored = f.store.get(&new.id).await.expect("leaving_running: get");
    assert_eq!(stored.state, JobState::Completed, "leaving_running: state");
    assert_eq!(stored.lease, None, "leaving_running: lease not cleared");
    assert_code(
        f.store.save(&claimed, &worker(1)).await,
        ErrorCode::Conflict,
        "leaving_running_clears_the_lease: save after completion",
    );
    assert_code(
        f.store.renew_lease(&new.id, &worker(1), LEASE).await,
        ErrorCode::Conflict,
        "leaving_running_clears_the_lease: renew after completion",
    );
    let claim = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("leaving_running: claim");
    assert_eq!(claim, None, "leaving_running: a completed job was claimed");
}

async fn expired_lease_can_be_reclaimed<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    f.store.insert(&new).await.expect("expired_lease: insert");
    let first = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("expired_lease: claim")
        .expect("expired_lease: no job");

    f.clock.advance(Duration::from_secs(29));
    let early = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("expired_lease: claim");
    assert_eq!(
        early, None,
        "expired_lease: claimed a job with a live lease"
    );

    f.clock.advance(Duration::from_secs(2));
    assert_code(
        f.store.renew_lease(&new.id, &worker(1), LEASE).await,
        ErrorCode::Conflict,
        "expired_lease_can_be_reclaimed: renewal after expiry",
    );
    let reclaimed = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("expired_lease: claim")
        .expect("expired_lease: the expired job was not reclaimed");
    assert_eq!(reclaimed.id, new.id, "expired_lease: wrong job");
    assert_eq!(
        reclaimed.attempt, 2,
        "expired_lease: attempt not incremented"
    );
    assert_eq!(
        reclaimed.lease.map(|l| l.holder),
        Some(worker(2)),
        "expired_lease: new holder"
    );
    assert_code(
        f.store.save(&first, &worker(1)).await,
        ErrorCode::Conflict,
        "expired_lease_can_be_reclaimed: save by the previous holder",
    );
}

async fn wake_follows_the_wake_condition<S: JobStore>(f: Fixture<S>) {
    let now = f.clock.now();
    for (job_id, run_id) in [("job-1", "run-1"), ("job-2", "run-2"), ("job-3", "run-3")] {
        f.store
            .insert(&job(job_id, run_id, now))
            .await
            .expect("wake: insert");
    }

    // job-1 waits for appr-1; job-2 waits for appr-2; job-3 keeps running.
    for (n, approval) in [(1, "appr-1"), (2, "appr-2")] {
        let mut claimed = f
            .store
            .claim(&worker(n), LEASE)
            .await
            .expect("wake: claim")
            .expect("wake: no job");
        claimed.state = JobState::Suspended;
        claimed.stop_reason = Some(StopReason::ApprovalRequired);
        claimed.wake_condition = Some(WakeCondition::ApprovalAnswered(id(approval)));
        f.store
            .save(&claimed, &worker(n))
            .await
            .expect("wake: suspend");
    }
    let running = f
        .store
        .claim(&worker(3), LEASE)
        .await
        .expect("wake: claim")
        .expect("wake: no job");

    let job1: JobId = id("job-1");
    let other = WakeReason::ApprovalAnswered(id("appr-2"));
    assert!(
        !f.store.wake(&job1, &other).await.expect("wake"),
        "wake: woke on a different approval"
    );
    let stored = f.store.get(&job1).await.expect("wake: get");
    assert_eq!(
        stored.state,
        JobState::Suspended,
        "wake: state changed without waking"
    );

    let matching = WakeReason::ApprovalAnswered(id("appr-1"));
    assert!(
        f.store.wake(&job1, &matching).await.expect("wake"),
        "wake: did not wake on the matching approval"
    );
    let stored = f.store.get(&job1).await.expect("wake: get");
    assert_eq!(
        stored.state,
        JobState::Queued,
        "wake: not queued after waking"
    );
    assert_eq!(
        stored.wake_condition, None,
        "wake: wake condition not cleared"
    );
    let reclaimed = f
        .store
        .claim(&worker(4), LEASE)
        .await
        .expect("wake: claim")
        .expect("wake: woken job is not claimable");
    assert_eq!(reclaimed.id, job1, "wake: wrong job claimed");

    assert!(
        f.store
            .wake(&id("job-2"), &WakeReason::CancelRequested)
            .await
            .expect("wake"),
        "wake: a cancel did not wake a suspended job"
    );
    assert!(
        !f.store
            .wake(&running.id, &WakeReason::CancelRequested)
            .await
            .expect("wake"),
        "wake: woke a running job"
    );
}
