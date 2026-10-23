//! The one retry loop of every model call: bounded, with exponential backoff,
//! honoring a provider's `retry-after`, and stopped by cancellation.

use std::future::Future;
use std::time::Duration;

use contracts::types::ModelRetryPolicy;
use contracts::{Error, ErrorCode, ExecutionContext};

/// A failed attempt, with the wait the provider asked for, if any.
#[derive(Debug)]
pub(crate) struct AttemptError {
    pub error: Error,
    pub retry_after: Option<Duration>,
}

impl From<Error> for AttemptError {
    fn from(error: Error) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// A value and the attempts it took.
#[derive(Debug)]
pub(crate) struct Retried<T> {
    pub value: T,
    pub attempts: u32,
}

/// Runs `attempt` until it succeeds, fails with an error that is not
/// retryable, or `policy.max_attempts` attempts have run. Each attempt and
/// each wait between attempts ends with `Cancelled` when `ctx` is cancelled.
/// `attempt` receives the attempt number, starting at 1.
pub(crate) async fn with_retries<T, F, Fut>(
    policy: &ModelRetryPolicy,
    ctx: &ExecutionContext,
    mut attempt: F,
) -> Result<Retried<T>, Error>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    let max_attempts = u32::try_from(policy.max_attempts.get()).unwrap_or(u32::MAX);
    let mut number = 1;
    loop {
        ctx.ensure_not_cancelled()?;
        let outcome = tokio::select! {
            biased;
            () = ctx.cancelled() => return Err(cancelled(ctx)),
            outcome = attempt(number) => outcome,
        };
        let failure = match outcome {
            Ok(value) => {
                return Ok(Retried {
                    value,
                    attempts: number,
                });
            }
            Err(failure) => failure,
        };
        if !failure.error.retryable() || number >= max_attempts {
            return Err(if number > 1 {
                Error::new(
                    failure.error.code(),
                    format!(
                        "{} (gave up after {number} attempts)",
                        failure.error.message()
                    ),
                )
                .with_source(failure.error)
            } else {
                failure.error
            });
        }
        let delay = backoff(policy, number, failure.retry_after);
        tracing::warn!(
            attempt = number,
            max_attempts,
            code = %failure.error.code(),
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "model call failed; retrying"
        );
        tokio::select! {
            biased;
            () = ctx.cancelled() => return Err(cancelled(ctx)),
            () = tokio::time::sleep(delay) => {}
        }
        number += 1;
    }
}

/// The wait after failed attempt `number`: `initial * 2^(number - 1)`, or the
/// provider's `retry_after`, never more than `max_backoff_ms`.
pub(crate) fn backoff(
    policy: &ModelRetryPolicy,
    number: u32,
    retry_after: Option<Duration>,
) -> Duration {
    let max = Duration::from_millis(policy.max_backoff_ms);
    let exponential = Duration::from_millis(policy.initial_backoff_ms)
        .saturating_mul(2u32.saturating_pow(number.saturating_sub(1)));
    retry_after.unwrap_or(exponential).min(max)
}

fn cancelled(ctx: &ExecutionContext) -> Error {
    Error::new(ErrorCode::Cancelled, "model call cancelled")
        .with_correlation_id(ctx.correlation_id().clone())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicU32, Ordering};

    use contracts::ids::CorrelationId;

    use super::*;

    fn policy(max_attempts: u64) -> ModelRetryPolicy {
        ModelRetryPolicy {
            max_attempts: NonZeroU64::new(max_attempts).expect("non-zero"),
            initial_backoff_ms: 1,
            max_backoff_ms: 4,
        }
    }

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(CorrelationId::try_from("corr-retry").expect("valid id"))
    }

    fn unavailable() -> AttemptError {
        Error::new(ErrorCode::Unavailable, "overloaded").into()
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let p = ModelRetryPolicy {
            max_attempts: NonZeroU64::new(5).expect("non-zero"),
            initial_backoff_ms: 100,
            max_backoff_ms: 300,
        };
        assert_eq!(backoff(&p, 1, None), Duration::from_millis(100));
        assert_eq!(backoff(&p, 2, None), Duration::from_millis(200));
        assert_eq!(backoff(&p, 3, None), Duration::from_millis(300));
        assert_eq!(backoff(&p, 30, None), Duration::from_millis(300));
        assert_eq!(
            backoff(&p, 1, Some(Duration::from_millis(250))),
            Duration::from_millis(250)
        );
        assert_eq!(
            backoff(&p, 1, Some(Duration::from_secs(60))),
            Duration::from_millis(300)
        );
    }

    #[tokio::test]
    async fn retryable_errors_are_retried_up_to_the_limit() {
        let calls = AtomicU32::new(0);
        let result: Result<Retried<()>, Error> = with_retries(&policy(3), &ctx(), |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(unavailable())
        })
        .await;
        let error = result.expect_err("every attempt fails");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(error.message().contains("after 3 attempts"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn success_after_a_failure_reports_the_attempts() {
        let result = with_retries(&policy(3), &ctx(), |n| async move {
            if n == 1 { Err(unavailable()) } else { Ok(n) }
        })
        .await
        .expect("second attempt succeeds");
        assert_eq!((result.value, result.attempts), (2, 2));
    }

    #[tokio::test]
    async fn other_errors_are_not_retried() {
        let calls = AtomicU32::new(0);
        let result: Result<Retried<()>, Error> = with_retries(&policy(3), &ctx(), |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(ErrorCode::InvalidInput, "bad request").into())
        })
        .await;
        assert_eq!(
            result.map_err(|e| e.code()).err(),
            Some(ErrorCode::InvalidInput)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_stops_a_wait() {
        let p = ModelRetryPolicy {
            max_attempts: NonZeroU64::new(3).expect("non-zero"),
            initial_backoff_ms: 60_000,
            max_backoff_ms: 60_000,
        };
        let ctx = ctx();
        let canceller = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let result: Result<Retried<()>, Error> = tokio::time::timeout(
            Duration::from_secs(5),
            with_retries(&p, &ctx, |_| async { Err(unavailable()) }),
        )
        .await
        .expect("cancellation ends the wait");
        assert_eq!(
            result.map_err(|e| e.code()).err(),
            Some(ErrorCode::Cancelled)
        );
    }
}
