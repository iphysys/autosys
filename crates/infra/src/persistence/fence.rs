//! The fence check (ADR-0032), shared by every fenced write.

use contracts::{Error, FenceToken, FoundLease, StaleFence, lease_lost};
use sqlx::PgConnection;
use sqlx::postgres::PgExecutor;

use super::error::{corrupt, db_error, from_bigint, to_bigint};

/// The first statement of every fenced write's transaction. If the fence is
/// current, takes a share lock on its job row, which a claim (`FOR UPDATE
/// SKIP LOCKED`), a renewal and a save all conflict with: until this
/// transaction ends, no change to the lease commits, so no write under an
/// earlier epoch commits after a claim. LeaseLost otherwise.
pub(crate) async fn verify_fence(
    conn: &mut PgConnection,
    fence: &FenceToken,
    op: &str,
) -> Result<(), Error> {
    let epoch = to_bigint(fence.epoch(), "lease epoch")?;
    let current = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "current!" FROM jobs
        WHERE id = $1 AND lease_holder = $2 AND lease_epoch = $3
            AND state = 'running' AND lease_expires_at > autosys_now()
        FOR SHARE
        "#,
        fence.job_id().as_str(),
        fence.holder().as_str(),
        epoch,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| db_error(op, e))?;
    match current {
        Some(_) => Ok(()),
        None => Err(refused(conn, fence, op).await),
    }
}

/// The error for a fence the database refused, filled in by an unlocked read
/// of the job as it is now. Logged, since the worker drops the run.
pub(crate) async fn refused<'e>(
    executor: impl PgExecutor<'e>,
    fence: &FenceToken,
    op: &str,
) -> Error {
    let row = sqlx::query!(
        "SELECT lease_epoch, lease_holder, state FROM jobs WHERE id = $1",
        fence.job_id().as_str(),
    )
    .fetch_optional(executor)
    .await;
    let found = match row {
        Ok(row) => row.map(|r| -> Result<FoundLease, Error> {
            Ok(FoundLease {
                epoch: from_bigint(r.lease_epoch, "lease epoch")?,
                holder: r
                    .lease_holder
                    .map(|h| h.parse())
                    .transpose()
                    .map_err(|_| corrupt("a job has an invalid lease holder"))?,
                running: r.state == "running",
            })
        }),
        Err(e) => return db_error(op, e),
    };
    let found = match found.transpose() {
        Ok(found) => found,
        Err(e) => return e,
    };
    let stale = StaleFence::new(fence, found);
    tracing::warn!(
        job_id = fence.job_id().as_str(),
        holder = fence.holder().as_str(),
        epoch = fence.epoch(),
        current_epoch = ?stale.current_epoch,
        current_holder = ?stale.current_holder.as_ref().map(|h| h.as_str()),
        staleness = ?stale.staleness,
        "{op}: stale fence refused"
    );
    lease_lost(stale)
}
