use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use contracts::ids::{Identifier, JobId};
use contracts::types::JobStatus;
use contracts::{Clock, ErrorCode, JobStore, UnfencedReason, WakeReason};

use super::{assert_code, unfenced};
use crate::builders::{self, checkpoint, id, job};
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

async fn insert<S: JobStore>(store: &S, job: &contracts::types::Job, case: &str) {
    store
        .insert(job, Some(&unfenced(UnfencedReason::JobInsert)))
        .await
        .unwrap_or_else(|e| panic!("{case}: insert: {e}"));
}

async fn wake<S: JobStore>(store: &S, job_id: &JobId, reason: &WakeReason) -> bool {
    store
        .wake(job_id, reason, Some(&unfenced(UnfencedReason::JobWake)))
        .await
        .expect("wake")
}

async fn insert_then_get<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    insert(f.store.as_ref(), &new, "insert_then_get").await;
    let stored = f.store.get(&new.id).await.expect("insert_then_get: get");
    assert_eq!(stored, new, "insert_then_get: stored job differs");
}

async fn duplicate_insert_is_conflict<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    insert(f.store.as_ref(), &new, "duplicate_insert").await;
    assert_code(
        f.store
            .insert(&new, Some(&unfenced(UnfencedReason::JobInsert)))
            .await,
        ErrorCode::Conflict,
        "duplicate_insert_is_conflict",
    );
}

async fn insert_requires_a_queued_job<S: JobStore>(f: Fixture<S>) {
    let mut running = job("job-1", "run-1", f.clock.now());
    running.status = JobStatus::Running;
    assert_code(
        f.store
            .insert(&running, Some(&unfenced(UnfencedReason::JobInsert)))
            .await,
        ErrorCode::InvalidInput,
        "insert_requires_a_queued_job",
    );
    // A new job has no checkpoint to resume from.
    let mut resuming = job("job-1", "run-1", f.clock.now());
    resuming.status = JobStatus::Queued {
        checkpoint: Some(checkpoint(1)),
    };
    assert_code(
        f.store
            .insert(&resuming, Some(&unfenced(UnfencedReason::JobInsert)))
            .await,
        ErrorCode::InvalidInput,
        "insert_requires_a_queued_job: with a checkpoint",
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
        f.store
            .wake(
                &missing,
                &WakeReason::CancelRequested,
                Some(&unfenced(UnfencedReason::JobWake)),
            )
            .await,
        ErrorCode::NotFound,
        "missing_job_is_not_found: wake",
    );
}

async fn find_by_run<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    insert(f.store.as_ref(), &new, "find_by_run").await;
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
    insert(f.store.as_ref(), &newer, "claim_oldest").await;
    insert(f.store.as_ref(), &older, "claim_oldest").await;

    let first = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("claim_oldest: claim")
        .expect("claim_oldest: a queued job was not claimed");
    assert_eq!(first.job.id, older.id, "claim_oldest: not the oldest job");
    assert_eq!(first.job.status, JobStatus::Running, "claim_oldest: status");
    assert_eq!(first.job.attempt, 1, "claim_oldest: attempt");
    let lease = first.job.lease.clone().expect("claim_oldest: no lease");
    assert_eq!(lease.holder, worker(1), "claim_oldest: lease holder");
    assert_eq!(
        lease.expires_at.0,
        now + TimeDelta::seconds(30),
        "claim_oldest: lease expiry"
    );
    assert_eq!(first.fence.job_id(), &older.id, "claim_oldest: fence job");
    assert_eq!(
        first.fence.holder(),
        &worker(1),
        "claim_oldest: fence holder"
    );
    let stored = f.store.get(&older.id).await.expect("claim_oldest: get");
    assert_eq!(stored, first.job, "claim_oldest: claim was not persisted");

    let second = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("claim_oldest: claim");
    assert_eq!(
        second.map(|c| c.job.id),
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
        insert(f.store.as_ref(), &new, "concurrent_claimers").await;
    }
    let mut handles = Vec::new();
    for n in 0..CLAIMERS {
        let store = Arc::clone(&f.store);
        handles.push(tokio::spawn(async move {
            let holder = worker(n);
            let mut claimed = Vec::new();
            while let Some(claim) = store
                .claim(&holder, LEASE)
                .await
                .expect("concurrent_claimers: claim")
            {
                claimed.push(claim.job.id.as_str().to_owned());
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
    insert(f.store.as_ref(), &new, "lease_renewal").await;
    let first = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("lease_renewal: claim")
        .expect("lease_renewal: no job");
    f.clock.advance(Duration::from_secs(10));
    let renewed = f
        .store
        .renew_lease(&first.fence, LEASE)
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

    f.clock.advance(Duration::from_secs(31));
    let second = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("lease_renewal: claim")
        .expect("lease_renewal: the expired job was not reclaimed");
    assert_code(
        f.store.renew_lease(&first.fence, LEASE).await,
        ErrorCode::LeaseLost,
        "lease_renewal: renewal with a stale fence",
    );
    f.store
        .renew_lease(&second.fence, LEASE)
        .await
        .expect("lease_renewal: the current fence renews");
}

async fn save_requires_the_lease<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    insert(f.store.as_ref(), &new, "save_requires_lease").await;
    let first = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("save_requires_lease: claim")
        .expect("save_requires_lease: no job");
    f.clock.advance(Duration::from_secs(31));
    let second = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("save_requires_lease: claim")
        .expect("save_requires_lease: no job");
    let mut progressed = second.job.clone();
    progressed.progress.steps_completed = 3;
    assert_code(
        f.store.save(&progressed, &first.fence).await,
        ErrorCode::LeaseLost,
        "save_requires_the_lease: save with a stale fence",
    );
    f.store
        .save(&progressed, &second.fence)
        .await
        .expect("save_requires_lease: save with the current fence");
    let stored = f
        .store
        .get(&new.id)
        .await
        .expect("save_requires_lease: get");
    assert_eq!(
        stored.progress.steps_completed, 3,
        "save_requires_lease: progress lost"
    );

    let mut other = stored.clone();
    other.id = id("job-2");
    assert_code(
        f.store.save(&other, &second.fence).await,
        ErrorCode::InvalidInput,
        "save_requires_the_lease: a fence for another job",
    );

    // The store owns the lease and attempt count; a caller cannot rewrite them.
    let mut forged = stored.clone();
    forged.attempt = 99;
    forged.lease = None;
    f.store
        .save(&forged, &second.fence)
        .await
        .expect("save_requires_lease: save");
    let stored = f
        .store
        .get(&new.id)
        .await
        .expect("save_requires_lease: get");
    assert_eq!(
        stored.attempt, 2,
        "save_requires_lease: attempt was overwritten"
    );
    assert_eq!(
        stored.lease.map(|l| l.holder),
        Some(worker(2)),
        "save_requires_lease: lease was overwritten"
    );
}

async fn leaving_running_clears_the_lease<S: JobStore>(f: Fixture<S>) {
    let new = job("job-1", "run-1", f.clock.now());
    insert(f.store.as_ref(), &new, "leaving_running").await;
    let claimed = f
        .store
        .claim(&worker(1), LEASE)
        .await
        .expect("leaving_running: claim")
        .expect("leaving_running: no job");
    let mut completed = claimed.job.clone();
    completed.status = builders::completed("success", 1);
    f.store
        .save(&completed, &claimed.fence)
        .await
        .expect("leaving_running: save");
    let stored = f.store.get(&new.id).await.expect("leaving_running: get");
    assert_eq!(stored.status, completed.status, "leaving_running: status");
    assert_eq!(stored.lease, None, "leaving_running: lease not cleared");
    assert_code(
        f.store.save(&completed, &claimed.fence).await,
        ErrorCode::LeaseLost,
        "leaving_running_clears_the_lease: save after completion",
    );
    assert_code(
        f.store.renew_lease(&claimed.fence, LEASE).await,
        ErrorCode::LeaseLost,
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
    insert(f.store.as_ref(), &new, "expired_lease").await;
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
        f.store.renew_lease(&first.fence, LEASE).await,
        ErrorCode::LeaseLost,
        "expired_lease_can_be_reclaimed: renewal after expiry",
    );
    let reclaimed = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("expired_lease: claim")
        .expect("expired_lease: the expired job was not reclaimed");
    assert_eq!(reclaimed.job.id, new.id, "expired_lease: wrong job");
    assert_eq!(
        reclaimed.job.attempt, 2,
        "expired_lease: attempt not incremented"
    );
    assert_eq!(
        reclaimed.job.lease.map(|l| l.holder),
        Some(worker(2)),
        "expired_lease: new holder"
    );
    assert_code(
        f.store.save(&first.job, &first.fence).await,
        ErrorCode::LeaseLost,
        "expired_lease_can_be_reclaimed: save by the previous holder",
    );
}

async fn wake_follows_the_wake_condition<S: JobStore>(f: Fixture<S>) {
    let now = f.clock.now();
    for (job_id, run_id) in [("job-1", "run-1"), ("job-2", "run-2"), ("job-3", "run-3")] {
        insert(f.store.as_ref(), &job(job_id, run_id, now), "wake").await;
    }

    // job-1 waits for appr-1; job-2 waits for appr-2; job-3 keeps running.
    for (n, approval) in [(1, "appr-1"), (2, "appr-2")] {
        let claimed = f
            .store
            .claim(&worker(n), LEASE)
            .await
            .expect("wake: claim")
            .expect("wake: no job");
        let mut suspended = claimed.job.clone();
        suspended.status = builders::suspended(approval, 1);
        f.store
            .save(&suspended, &claimed.fence)
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
        !wake(f.store.as_ref(), &job1, &other).await,
        "wake: woke on a different approval"
    );
    let stored = f.store.get(&job1).await.expect("wake: get");
    assert_eq!(
        stored.status,
        builders::suspended("appr-1", 1),
        "wake: status changed without waking"
    );

    let matching = WakeReason::ApprovalAnswered(id("appr-1"));
    assert!(
        wake(f.store.as_ref(), &job1, &matching).await,
        "wake: did not wake on the matching approval"
    );
    let stored = f.store.get(&job1).await.expect("wake: get");
    assert_eq!(
        stored.status,
        JobStatus::Queued {
            checkpoint: Some(checkpoint(1))
        },
        "wake: not queued on its checkpoint after waking"
    );
    let reclaimed = f
        .store
        .claim(&worker(4), LEASE)
        .await
        .expect("wake: claim")
        .expect("wake: woken job is not claimable");
    assert_eq!(reclaimed.job.id, job1, "wake: wrong job claimed");

    assert!(
        wake(f.store.as_ref(), &id("job-2"), &WakeReason::CancelRequested).await,
        "wake: a cancel did not wake a suspended job"
    );
    assert!(
        !wake(
            f.store.as_ref(),
            &running.job.id,
            &WakeReason::CancelRequested
        )
        .await,
        "wake: woke a running job"
    );
}
