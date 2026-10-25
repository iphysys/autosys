//! Job claiming across processes, and lease times on the database clock: PR
//! 4's second done-when test. Needs `DATABASE_URL` (see README, section
//! Database).

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};
use contracts::ids::Identifier;
use contracts::{JobStore, UnfencedReason, WriteAuthority};
use infra::persistence::PgJobStore;
use support::TestDb;
use testkit::builders::{id, job};

const LEASE: Duration = Duration::from_secs(30);
/// Submitting a job is a write from outside any job.
const INSERT: Option<&WriteAuthority> = Some(&WriteAuthority::Unfenced(UnfencedReason::JobInsert));

/// Two pools stand in for two worker processes, each running several claim
/// loops at once, all racing for the same jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_workers_never_claim_the_same_job() {
    const JOBS: usize = 200;
    const WORKERS: usize = 2;
    const LOOPS_PER_WORKER: usize = 4;

    let db = TestDb::create().await;
    let seed = PgJobStore::new(db.database());
    let created = Utc::now();
    for n in 0..JOBS {
        let new = job(&format!("job-{n:03}"), &format!("run-{n:03}"), created);
        seed.insert(&new, INSERT).await.expect("insert");
    }

    let mut loops = Vec::new();
    for w in 0..WORKERS {
        let store = Arc::new(PgJobStore::new(
            db.second_process(LOOPS_PER_WORKER as u32).await,
        ));
        for l in 0..LOOPS_PER_WORKER {
            let store = Arc::clone(&store);
            let holder: Identifier = id(&format!("worker-{w}-loop-{l}"));
            loops.push(tokio::spawn(async move {
                let mut claimed = Vec::new();
                while let Some(claim) = store.claim(&holder, LEASE).await.expect("claim") {
                    let job = claim.job;
                    let lease = job.lease.as_ref().expect("a claimed job has a lease");
                    assert_eq!(lease.holder, holder, "lease holder");
                    assert_eq!(job.attempt, 1, "{} was claimed before", job.id.as_str());
                    claimed.push(job.id.as_str().to_owned());
                }
                claimed
            }));
        }
    }

    let mut all = Vec::new();
    for handle in loops {
        all.extend(handle.await.expect("a claim loop panicked"));
    }
    let unique: HashSet<&String> = all.iter().collect();
    assert_eq!(unique.len(), all.len(), "a job was claimed twice");
    assert_eq!(all.len(), JOBS, "not every job was claimed");
    db.drop().await;
}

/// The store sets lease times from `autosys_now()`, not from the host clock:
/// with the database clock stopped in 2100, the lease says 2100.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_times_come_from_the_database_clock() {
    let db = TestDb::create().await;
    let stopped_at = Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap();
    db.stop_clock_at(stopped_at).await;
    let store = PgJobStore::new(db.database());
    store
        .insert(&job("job-1", "run-1", Utc::now()), INSERT)
        .await
        .expect("insert");

    let claimed = store
        .claim(&id("worker-1"), LEASE)
        .await
        .expect("claim")
        .expect("no job claimed")
        .job;
    let lease = claimed.lease.expect("no lease");
    assert_eq!(lease.acquired_at.0, stopped_at, "acquired_at");
    assert_eq!(lease.heartbeat_at.0, stopped_at, "heartbeat_at");
    assert_eq!(
        lease.expires_at.0,
        stopped_at + TimeDelta::seconds(30),
        "expires_at"
    );
    assert_eq!(claimed.updated_at.0, stopped_at, "updated_at");
    db.drop().await;
}

/// With the production `autosys_now()`, which is `now()`, a lease expires
/// when the database's real time passes its expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_expire_on_the_real_database_clock() {
    let db = TestDb::create().await;
    let store = PgJobStore::new(db.database());
    store
        .insert(&job("job-1", "run-1", Utc::now()), INSERT)
        .await
        .expect("insert");

    let before = db.now().await;
    let claimed = store
        .claim(&id("worker-1"), Duration::from_secs(2))
        .await
        .expect("claim")
        .expect("no job claimed")
        .job;
    let after = db.now().await;
    let lease = claimed.lease.expect("no lease");
    assert!(
        before <= lease.acquired_at.0 && lease.acquired_at.0 <= after,
        "the lease was not taken at the database's current time"
    );
    assert_eq!(
        lease.expires_at.0 - lease.acquired_at.0,
        TimeDelta::seconds(2),
        "lease length"
    );

    let early = store.claim(&id("worker-2"), LEASE).await.expect("claim");
    assert_eq!(early, None, "claimed a job with a live lease");

    tokio::time::sleep(Duration::from_millis(2500)).await;
    let reclaimed = store
        .claim(&id("worker-2"), LEASE)
        .await
        .expect("claim")
        .expect("the expired lease was not reclaimed");
    assert_eq!(reclaimed.job.attempt, 2, "attempt");
    assert_eq!(reclaimed.fence.epoch(), 2, "lease epoch");
    db.drop().await;
}
