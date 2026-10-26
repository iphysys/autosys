//! One contract-test suite per trait in `synth-contracts`.
//!
//! Each suite is an async function that panics, naming the failed case, when
//! an implementation breaks the contract. Store suites take a factory and build
//! a fresh store for every case, so cases never see each other's data. Run them
//! from a multi-threaded tokio test, for example:
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread")]
//! async fn in_memory_job_store() {
//!     testkit::contract::job_store(|clock| async move { InMemoryJobStore::new(clock) }).await;
//! }
//! ```
//!
//! The job-store suite controls time through the clock it passes to `make`.
//! When a store reads time elsewhere, such as the database clock (ADR-0023),
//! its test makes that time follow the given clock.

mod artifact_store;
mod check;
mod event_log;
mod event_subscriber;
mod evidence_store;
mod fencing;
mod gate;
mod job_store;
mod model_provider;
mod replay_store;
mod run_reports;
mod run_submitter;
mod sdr_repository;
mod step_store;
mod tool;

pub use artifact_store::artifact_store;
pub use check::check;
pub use event_log::event_log;
pub use event_subscriber::event_subscriber;
pub use evidence_store::evidence_store;
pub use fencing::{FencedStores, fencing};
pub use gate::gate;
pub use job_store::job_store;
pub use model_provider::{model_provider, model_provider_tool_round_trip};
pub use replay_store::replay_store;
pub use run_reports::{ReportsWorld, run_reports};
pub use run_submitter::{SubmitterWorld, run_submitter};
pub use sdr_repository::sdr_repository;
pub use step_store::step_store;
pub use tool::tool;

use std::fmt::Debug;
use std::time::Duration;

use contracts::{
    Claimed, Error, ErrorCode, ExecutionContext, JobStore, UnfencedReason, WriteAuthority,
};

use crate::builders::{id, job};
use crate::clock::ManualClock;
use crate::leases::{HOLDER, LONG_LEASE};

/// Upper bound for anything a suite waits on, so a broken implementation
/// fails instead of hanging.
const WAIT: Duration = Duration::from_secs(5);

fn ctx() -> ExecutionContext {
    ExecutionContext::new(id("corr-contract"))
}

fn unfenced(reason: UnfencedReason) -> WriteAuthority {
    WriteAuthority::Unfenced(reason)
}

fn fenced(claimed: &Claimed) -> WriteAuthority {
    WriteAuthority::Fenced(claimed.fence.clone())
}

/// Inserts job `job-1` of `run-1` into `jobs` and claims it for a year, so
/// the suite's writes have a current fence while it advances `clock`.
async fn lease_one<J: JobStore>(jobs: &J, clock: &ManualClock) -> Claimed {
    use contracts::Clock;
    jobs.insert(
        &job("job-1", "run-1", clock.now()),
        Some(&unfenced(UnfencedReason::JobSubmit)),
    )
    .await
    .expect("insert the job that fences the suite");
    jobs.claim(&id(HOLDER), LONG_LEASE)
        .await
        .expect("claim the job that fences the suite")
        .expect("the suite's job is claimable")
}

fn cancelled_ctx() -> ExecutionContext {
    let ctx = ctx();
    ctx.cancel();
    ctx
}

#[track_caller]
fn assert_code<T: Debug>(result: Result<T, Error>, code: ErrorCode, case: &str) {
    match result {
        Err(e) => assert_eq!(e.code(), code, "{case}: expected {code}, got {e}"),
        Ok(value) => panic!("{case}: expected {code}, got Ok({value:?})"),
    }
}
