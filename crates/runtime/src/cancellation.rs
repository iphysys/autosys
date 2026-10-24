//! Timeouts and cancellation for the runtime's own I/O, and the wall-time
//! deadline of a run (blueprint section 13.1).
//!
//! Every await on a store or a component has a timeout. Most also stop when
//! the run is cancelled ([`bounded`]). What must be written even then, such
//! as a decided commit or `run_stopped`, has the timeout alone ([`timed`]).

use std::future::Future;
use std::time::Duration;

use contracts::{Error, ErrorCode, ExecutionContext};
use tokio::time::Instant;

use crate::error::RuntimeError;

/// Runs `future` until it finishes, `limit` passes, or `ctx` is cancelled.
/// Dropping the future at the limit cancels it.
pub(crate) async fn bounded<T>(
    ctx: &ExecutionContext,
    limit: Duration,
    operation: &'static str,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, RuntimeError> {
    tokio::select! {
        biased;
        () = ctx.cancelled() => Err(RuntimeError::Cancelled { operation }),
        finished = timed(limit, operation, future) => finished,
    }
}

/// Runs `future` until it finishes or `limit` passes, whatever happens to
/// the run's cancellation.
pub(crate) async fn timed<T>(
    limit: Duration,
    operation: &'static str,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, RuntimeError> {
    match tokio::time::timeout(limit, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) if e.code() == ErrorCode::Cancelled => {
            Err(RuntimeError::Cancelled { operation })
        }
        Ok(Err(source)) => Err(RuntimeError::Component { operation, source }),
        Err(_) => Err(RuntimeError::Timeout { operation, limit }),
    }
}

/// The wall-time budget of a run: a monotonic clock, started when the run
/// starts or resumes, plus the wall time the job used before. Time spent
/// waiting for an approval is not counted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    started: Instant,
    used_before: f64,
    at: Instant,
}

impl Deadline {
    pub(crate) fn new(max_seconds: u64, used_seconds: f64) -> Self {
        let started = Instant::now();
        let used_before = if used_seconds.is_finite() && used_seconds > 0.0 {
            used_seconds
        } else {
            0.0
        };
        let left = (max_seconds as f64 - used_before).max(0.0);
        Self {
            started,
            used_before,
            at: started + Duration::from_secs_f64(left),
        }
    }

    pub(crate) fn passed(&self) -> bool {
        Instant::now() >= self.at
    }

    /// The job's wall time so far, this run's share included.
    pub(crate) fn used_seconds(&self) -> f64 {
        self.used_before + self.started.elapsed().as_secs_f64()
    }

    /// Runs `future` with `step` as its context. At the deadline `step` is
    /// cancelled and the future is still awaited, so calls in flight end as
    /// cancelled and their components finish their own bookkeeping, such as
    /// a tool's step record.
    pub(crate) async fn run<F: Future>(&self, step: &ExecutionContext, future: F) -> F::Output {
        tokio::pin!(future);
        tokio::select! {
            output = &mut future => output,
            () = tokio::time::sleep_until(self.at) => {
                step.cancel();
                future.await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new("corr-1".parse().expect("id"))
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_cancels_the_step_and_lets_the_call_finish() {
        let deadline = Deadline::new(10, 4.0);
        let step = ctx().for_step("step-1".parse().expect("id"));
        let observed = deadline
            .run(&step, async {
                tokio::select! {
                    () = step.cancelled() => "cancelled, and the call still returned",
                    () = tokio::time::sleep(Duration::from_secs(60)) => "finished",
                }
            })
            .await;
        assert_eq!(observed, "cancelled, and the call still returned");
        assert!(deadline.passed());
        assert!((deadline.used_seconds() - 10.0).abs() < 1e-6);
    }

    #[tokio::test(start_paused = true)]
    async fn wall_time_used_before_counts_and_a_spent_budget_has_passed() {
        let spent = Deadline::new(10, 12.5);
        assert!(spent.passed());
        let fresh = Deadline::new(10, f64::NAN);
        assert!(!fresh.passed());
        assert_eq!(fresh.used_seconds(), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_io_stops_on_cancellation_and_on_its_timeout() {
        let run = ctx();
        let slow = || async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<_, Error>(())
        };
        let timed_out = bounded(&run, Duration::from_secs(1), "reading", slow()).await;
        assert_eq!(timed_out.map_err(|e| e.code()), Err(ErrorCode::Timeout));

        run.cancel();
        let cancelled = bounded(&run, Duration::from_secs(1), "reading", slow()).await;
        assert_eq!(cancelled.map_err(|e| e.code()), Err(ErrorCode::Cancelled));
        // The bookkeeping kind ignores the cancellation.
        let written = timed(Duration::from_secs(120), "writing", slow()).await;
        assert!(written.is_ok());

        let failed = timed(Duration::from_secs(1), "writing", async {
            Err::<(), _>(Error::new(ErrorCode::Unavailable, "down"))
        })
        .await;
        assert_eq!(failed.map_err(|e| e.code()), Err(ErrorCode::Unavailable));
    }
}
