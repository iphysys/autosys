use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use contracts::ids::{Identifier, JobId};
use contracts::types::JobStatus;
use contracts::{
    Claimed, Clock, ErrorCode, JobStore, JobWrite, RecordWrite, UnfencedReason, WakeReason,
};

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
    a_claim_reports_an_interrupted_job(fresh(&make).await).await;
    the_continuation_round_trips_and_stays(fresh(&make).await).await;
    a_refused_save_leaves_the_record(fresh(&make).await).await;
    parked_jobs_carry_their_checked_seq(fresh(&make).await).await;
    a_retry_delay_holds_back_the_claim(fresh(&make).await).await;
}

async fn insert_and_claim<S: JobStore>(f: &Fixture<S>, job_id: &str, n: usize) -> Claimed {
    insert(
        f.store.as_ref(),
        &job(job_id, &format!("run-{job_id}"), f.clock.now()),
        job_id,
    )
    .await;
    f.store
        .claim(&worker(n), LEASE)
        .await
        .expect("claim")
        .expect("a job to claim")
}

async fn a_claim_reports_an_interrupted_job<S: JobStore>(f: Fixture<S>) {
    let first = insert_and_claim(&f, "job-1", 1).await;
    assert!(!first.interrupted, "interrupted: a queued job was not");
    f.clock.advance(LEASE + Duration::from_secs(1));
    let again = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("claim")
        .expect("the expired job is claimable");
    assert!(again.interrupted, "interrupted: an expired running job was");
}

async fn the_continuation_round_trips_and_stays<S: JobStore>(f: Fixture<S>) {
    let claimed = insert_and_claim(&f, "job-1", 1).await;
    let job_id = claimed.job.id.clone();
    assert_eq!(
        f.store.continuation(&job_id).await.expect("continuation"),
        None,
        "continuation: a new job has none"
    );
    let record = builders::continuation("run-job-1", 1);
    f.store
        .save(
            &JobWrite {
                job: &claimed.job,
                continuation: RecordWrite::Replace(&record),
                retry_after: None,
            },
            &claimed.fence,
        )
        .await
        .expect("continuation: save the record");
    assert_eq!(
        f.store.continuation(&job_id).await.expect("continuation"),
        Some(record.clone()),
        "continuation: the record, opaque blocks included, comes back as saved"
    );

    // A save that keeps the record, then parking and waking, leave it.
    let mut suspended = claimed.job.clone();
    suspended.status = builders::suspended("appr-1", 1);
    f.store
        .save(&JobWrite::keep(&suspended), &claimed.fence)
        .await
        .expect("continuation: suspend");
    assert!(
        wake(f.store.as_ref(), &job_id, &WakeReason::CancelRequested).await,
        "continuation: wake"
    );
    assert_eq!(
        f.store.continuation(&job_id).await.expect("continuation"),
        Some(record),
        "continuation: kept through a park and a wake-up"
    );
    assert_code(
        f.store.continuation(&id("job-missing")).await,
        ErrorCode::NotFound,
        "continuation: a missing job",
    );
}

async fn a_refused_save_leaves_the_record<S: JobStore>(f: Fixture<S>) {
    let first = insert_and_claim(&f, "job-1", 1).await;
    let kept = builders::continuation("run-job-1", 1);
    f.store
        .save(
            &JobWrite {
                job: &first.job,
                continuation: RecordWrite::Replace(&kept),
                retry_after: None,
            },
            &first.fence,
        )
        .await
        .expect("refused save: the first record");
    f.clock.advance(LEASE + Duration::from_secs(1));
    let second = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("claim")
        .expect("the expired job is claimable");
    let stale = builders::continuation("run-job-1", 2);
    assert_code(
        f.store
            .save(
                &JobWrite {
                    job: &first.job,
                    continuation: RecordWrite::Replace(&stale),
                    retry_after: None,
                },
                &first.fence,
            )
            .await,
        ErrorCode::LeaseLost,
        "refused save: a stale fence",
    );
    assert_eq!(
        f.store
            .continuation(&second.job.id)
            .await
            .expect("continuation"),
        Some(kept),
        "refused save: the record changed"
    );
}

async fn parked_jobs_carry_their_checked_seq<S: JobStore>(f: Fixture<S>) {
    for (n, (job_id, seq)) in [("job-1", 7), ("job-2", 9)].into_iter().enumerate() {
        let claimed = insert_and_claim(&f, job_id, n + 1).await;
        let mut parked = claimed.job.clone();
        parked.progress.last_event_seq = seq;
        parked.status = builders::suspended("appr-1", 1);
        f.clock.advance(Duration::from_secs(1));
        f.store
            .save(&JobWrite::keep(&parked), &claimed.fence)
            .await
            .expect("parked: suspend");
    }
    insert_and_claim(&f, "job-3", 3).await;

    let parked = f.store.parked(10).await.expect("parked");
    let listed: Vec<(&str, u64)> = parked
        .iter()
        .map(|p| (p.job.id.as_str(), p.checked_seq))
        .collect();
    assert_eq!(
        listed,
        [("job-1", 7), ("job-2", 9)],
        "parked: the parked jobs, least recently updated first, with the seq they were parked at"
    );
    assert_eq!(
        f.store.parked(1).await.expect("parked").len(),
        1,
        "parked: limit"
    );
    assert!(
        wake(f.store.as_ref(), &id("job-1"), &WakeReason::CancelRequested).await,
        "parked: wake"
    );
    let left: Vec<String> = f
        .store
        .parked(10)
        .await
        .expect("parked")
        .into_iter()
        .map(|p| p.job.id.as_str().to_owned())
        .collect();
    assert_eq!(left, ["job-2"], "parked: a woken job is no longer parked");
}

async fn a_retry_delay_holds_back_the_claim<S: JobStore>(f: Fixture<S>) {
    let claimed = insert_and_claim(&f, "job-1", 1).await;
    let mut running = claimed.job.clone();
    running.failed_attempts = 1;
    assert_code(
        f.store
            .save(
                &JobWrite {
                    job: &running,
                    continuation: RecordWrite::Keep,
                    retry_after: Some(Duration::from_secs(10)),
                },
                &claimed.fence,
            )
            .await,
        ErrorCode::InvalidInput,
        "retry delay: needs a queued job",
    );
    let mut queued = claimed.job.clone();
    queued.status = JobStatus::Queued { checkpoint: None };
    queued.failed_attempts = 1;
    f.store
        .save(
            &JobWrite {
                job: &queued,
                continuation: RecordWrite::Keep,
                retry_after: Some(Duration::from_secs(10)),
            },
            &claimed.fence,
        )
        .await
        .expect("retry delay: requeue");
    let stored = f.store.get(&queued.id).await.expect("get");
    assert_eq!(
        stored.not_before.map(|t| t.0),
        Some(f.clock.now() + TimeDelta::seconds(10)),
        "retry delay: not_before on the store's clock"
    );
    assert_eq!(stored.failed_attempts, 1, "retry delay: failed_attempts");
    assert!(
        f.store
            .claim(&worker(2), LEASE)
            .await
            .expect("claim")
            .is_none(),
        "retry delay: claimed before not_before"
    );
    f.clock.advance(Duration::from_secs(11));
    let reclaimed = f
        .store
        .claim(&worker(2), LEASE)
        .await
        .expect("claim")
        .expect("retry delay: not claimable after not_before");
    assert!(
        !reclaimed.interrupted,
        "retry delay: a requeued job was not interrupted"
    );
    assert_eq!(
        reclaimed.job.not_before, None,
        "retry delay: cleared by the claim"
    );
}

fn worker(n: usize) -> Identifier {
    id(&format!("worker-{n}"))
}

async fn insert<S: JobStore>(store: &S, job: &contracts::types::Job, case: &str) {
    store
        .insert(job, Some(&unfenced(UnfencedReason::JobSubmit)))
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
            .insert(&new, Some(&unfenced(UnfencedReason::JobSubmit)))
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
            .insert(&running, Some(&unfenced(UnfencedReason::JobSubmit)))
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
            .insert(&resuming, Some(&unfenced(UnfencedReason::JobSubmit)))
            .await,
        ErrorCode::InvalidInput,
        "insert_requires_a_queued_job: with a checkpoint",
    );
    // A delay comes only from a requeue's save.
    let mut delayed = job("job-1", "run-1", f.clock.now());
    delayed.not_before = Some(contracts::types::Timestamp(f.clock.now()));
    assert_code(
        f.store
            .insert(&delayed, Some(&unfenced(UnfencedReason::JobSubmit)))
            .await,
        ErrorCode::InvalidInput,
        "insert_requires_a_queued_job: with a delay",
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
        f.store
            .save(&JobWrite::keep(&progressed), &first.fence)
            .await,
        ErrorCode::LeaseLost,
        "save_requires_the_lease: save with a stale fence",
    );
    f.store
        .save(&JobWrite::keep(&progressed), &second.fence)
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
        f.store.save(&JobWrite::keep(&other), &second.fence).await,
        ErrorCode::InvalidInput,
        "save_requires_the_lease: a fence for another job",
    );

    // The store owns the lease and attempt count; a caller cannot rewrite them.
    let mut forged = stored.clone();
    forged.attempt = 99;
    forged.lease = None;
    f.store
        .save(&JobWrite::keep(&forged), &second.fence)
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
        .save(&JobWrite::keep(&completed), &claimed.fence)
        .await
        .expect("leaving_running: save");
    let stored = f.store.get(&new.id).await.expect("leaving_running: get");
    assert_eq!(stored.status, completed.status, "leaving_running: status");
    assert_eq!(stored.lease, None, "leaving_running: lease not cleared");
    assert_code(
        f.store
            .save(&JobWrite::keep(&completed), &claimed.fence)
            .await,
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
        f.store
            .save(&JobWrite::keep(&first.job), &first.fence)
            .await,
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
            .save(&JobWrite::keep(&suspended), &claimed.fence)
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
