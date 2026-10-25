//! The heartbeat of a claimed job (blueprint section 6.2, ADR-0031): renews
//! its lease, and gives up on it once renewals stop working.

use std::sync::Arc;
use std::time::Duration;

use contracts::{BackgroundTask, ErrorCode, FenceToken, JobStore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Renews the lease of `fence`'s job every `every` for `lease`. On
/// `lease_lost`, or when no renewal has succeeded for `lease - every`, it
/// cancels `lost` and stops: the run is dropped, never stopped, since
/// another worker may already hold the job (ADR-0032). Other errors are
/// retried at the next beat.
pub(crate) fn heartbeat(
    jobs: Arc<dyn JobStore>,
    fence: FenceToken,
    lease: Duration,
    every: Duration,
    lost: CancellationToken,
) -> BackgroundTask {
    Box::pin(async move {
        let give_up_after = lease.saturating_sub(every);
        let mut renewed = Instant::now();
        loop {
            tokio::select! {
                () = lost.cancelled() => return,
                () = tokio::time::sleep(every) => {}
            }
            match tokio::time::timeout(every, jobs.renew_lease(&fence, lease)).await {
                Ok(Ok(_)) => renewed = Instant::now(),
                Ok(Err(error)) if error.code() == ErrorCode::LeaseLost => {
                    tracing::warn!(%error, "the lease was lost");
                    lost.cancel();
                    return;
                }
                Ok(Err(error)) => tracing::warn!(%error, "a lease renewal failed"),
                Err(_) => tracing::warn!("a lease renewal timed out"),
            }
            if renewed.elapsed() >= give_up_after {
                tracing::warn!(
                    since = ?renewed.elapsed(),
                    "no lease renewal succeeded in time; giving the job up"
                );
                lost.cancel();
                return;
            }
        }
    })
}
