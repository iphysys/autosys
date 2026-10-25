use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use contracts::ids::{Identifier, JobId, RunId};
use contracts::types::{
    ContinuationRecord, Job, JobState, JobStatus, Lease, Timestamp, WakeCondition,
};
use contracts::{
    Claimed, Error, FenceToken, JobStore, JobWrite, Parked, RecordWrite, StoreWrite, WakeReason,
    WriteAuthority, authorize, job_state, wake_condition,
};
use sqlx::postgres::types::PgInterval;
use sqlx::types::Json;

use super::Database;
use super::error::{conflict, corrupt, db_error, from_bigint, invalid, not_found, to_bigint};
use super::fence::{refused, verify_fence};

/// How many times `claim` retries when it found nothing but claimable jobs
/// remain. See [`PgJobStore::claim`].
const CLAIM_ATTEMPTS: usize = 5;

/// [`JobStore`] on Postgres. Claims use `FOR UPDATE SKIP LOCKED`, so
/// concurrent claimers, in any number of processes, never receive the same
/// job. Lease and retry times come from the database clock. Each claim
/// raises `lease_epoch`, and renewals and saves match it in their `WHERE`
/// clause (ADR-0032). The job's status lives in the document and the `state`
/// column is generated from its tag, so a claim or a wake-up rewrites the
/// status there. The continuation record and the wake-up scan's checked
/// sequence number have columns of their own.
pub struct PgJobStore {
    db: Database,
}

impl PgJobStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// A `jobs` row: the job as last written, plus the columns the store manages.
struct JobRow {
    document: Json<Job>,
    attempt: i64,
    updated_at: DateTime<Utc>,
    not_before: Option<DateTime<Utc>>,
    lease_holder: Option<String>,
    lease_acquired_at: Option<DateTime<Utc>>,
    lease_expires_at: Option<DateTime<Utc>>,
    lease_heartbeat_at: Option<DateTime<Utc>>,
}

impl JobRow {
    fn into_job(self) -> Result<Job, Error> {
        let mut job = self.document.0;
        let id = job.id.as_str().to_owned();
        job.attempt = from_bigint(self.attempt, "attempt")?;
        job.updated_at = Timestamp(self.updated_at);
        job.not_before = self.not_before.map(Timestamp);
        job.lease = match (
            self.lease_holder,
            self.lease_acquired_at,
            self.lease_expires_at,
            self.lease_heartbeat_at,
        ) {
            (Some(holder), Some(acquired_at), Some(expires_at), Some(heartbeat_at)) => {
                Some(Lease {
                    holder: holder
                        .parse()
                        .map_err(|_| corrupt(format!("job {id} has an invalid lease holder")))?,
                    acquired_at: Timestamp(acquired_at),
                    expires_at: Timestamp(expires_at),
                    heartbeat_at: Timestamp(heartbeat_at),
                })
            }
            (None, None, None, None) => None,
            _ => return Err(corrupt(format!("job {id} has a partial lease"))),
        };
        Ok(job)
    }
}

/// A claimed row: the job, and whether it was running when claimed.
struct ClaimedRow {
    document: Json<Job>,
    attempt: i64,
    updated_at: DateTime<Utc>,
    lease_holder: Option<String>,
    lease_acquired_at: Option<DateTime<Utc>>,
    lease_expires_at: Option<DateTime<Utc>>,
    lease_heartbeat_at: Option<DateTime<Utc>>,
    lease_epoch: i64,
    interrupted: bool,
}

/// A parked row: the job, and the sequence number its scan starts after.
struct ParkedRow {
    document: Json<Job>,
    attempt: i64,
    updated_at: DateTime<Utc>,
    not_before: Option<DateTime<Utc>>,
    lease_holder: Option<String>,
    lease_acquired_at: Option<DateTime<Utc>>,
    lease_expires_at: Option<DateTime<Utc>>,
    lease_heartbeat_at: Option<DateTime<Utc>>,
    wake_checked_seq: i64,
}

fn interval(duration: Duration, what: &str) -> Result<PgInterval, Error> {
    PgInterval::try_from(duration).map_err(|_| invalid(format!("{what} is too long")))
}

fn lease_interval(lease_for: Duration) -> Result<PgInterval, Error> {
    interval(lease_for, "lease duration")
}

/// Whether `reason` wakes a job waiting on `condition`. A cancel wakes any
/// waiting or suspended job.
fn wakes(reason: &WakeReason, condition: Option<&WakeCondition>) -> bool {
    match (reason, condition) {
        (WakeReason::CancelRequested, _) => true,
        (
            WakeReason::ApprovalAnswered(answered),
            Some(WakeCondition::ApprovalAnswered(waiting)),
        ) => answered == waiting,
        (WakeReason::ApprovalAnswered(_), None) => false,
    }
}

#[async_trait]
impl JobStore for PgJobStore {
    async fn insert(&self, job: &Job, authority: Option<&WriteAuthority>) -> Result<(), Error> {
        let fence = authorize(authority, StoreWrite::JobInsert)?;
        if job.status != (JobStatus::Queued { checkpoint: None })
            || job.lease.is_some()
            || job.not_before.is_some()
        {
            return Err(invalid(
                "a new job must be queued, without a checkpoint, a delay or a lease",
            ));
        }
        let attempt = to_bigint(job.attempt, "attempt")?;
        self.db
            .bounded("insert job", async {
                let mut tx = self
                    .db
                    .pool
                    .begin()
                    .await
                    .map_err(|e| db_error("insert job", e))?;
                if let Some(fence) = fence {
                    verify_fence(&mut tx, fence, "insert job").await?;
                }
                let inserted = sqlx::query!(
                    r#"
                    INSERT INTO jobs (id, run_id, created_at, document, attempt, updated_at)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    ON CONFLICT (id) DO NOTHING
                    "#,
                    job.id.as_str(),
                    job.run_id.as_str(),
                    job.created_at.0,
                    Json(job) as _,
                    attempt,
                    job.updated_at.0,
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| db_error("insert job", e))?;
                if inserted.rows_affected() == 0 {
                    return Err(conflict(format!("job {} already exists", job.id.as_str())));
                }
                tx.commit().await.map_err(|e| db_error("insert job", e))?;
                Ok(())
            })
            .await
    }

    async fn get(&self, id: &JobId) -> Result<Job, Error> {
        self.db
            .bounded("get job", async {
                let row = sqlx::query_as!(
                    JobRow,
                    r#"
                    SELECT document AS "document!: Json<Job>", attempt, updated_at, not_before,
                        lease_holder, lease_acquired_at, lease_expires_at, lease_heartbeat_at
                    FROM jobs
                    WHERE id = $1
                    "#,
                    id.as_str(),
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("get job", e))?;
                row.ok_or_else(|| not_found("job", id.as_str()))?.into_job()
            })
            .await
    }

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error> {
        self.db
            .bounded("find job by run", async {
                let row = sqlx::query_as!(
                    JobRow,
                    r#"
                    SELECT document AS "document!: Json<Job>", attempt, updated_at, not_before,
                        lease_holder, lease_acquired_at, lease_expires_at, lease_heartbeat_at
                    FROM jobs
                    WHERE run_id = $1
                    ORDER BY created_at, id
                    LIMIT 1
                    "#,
                    run_id.as_str(),
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("find job by run", e))?;
                row.map(JobRow::into_job).transpose()
            })
            .await
    }

    async fn continuation(&self, id: &JobId) -> Result<Option<ContinuationRecord>, Error> {
        self.db
            .bounded("read the continuation", async {
                let row = sqlx::query_scalar!(
                    r#"
                    SELECT continuation AS "continuation: Json<ContinuationRecord>"
                    FROM jobs
                    WHERE id = $1
                    "#,
                    id.as_str(),
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("read the continuation", e))?;
                Ok(row
                    .ok_or_else(|| not_found("job", id.as_str()))?
                    .map(|record| record.0))
            })
            .await
    }

    async fn parked(&self, limit: usize) -> Result<Vec<Parked>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.db
            .bounded("list parked jobs", async {
                let rows = sqlx::query_as!(
                    ParkedRow,
                    r#"
                    SELECT document AS "document!: Json<Job>", attempt, updated_at, not_before,
                        lease_holder, lease_acquired_at, lease_expires_at, lease_heartbeat_at,
                        wake_checked_seq
                    FROM jobs
                    WHERE state IN ('waiting', 'suspended')
                    ORDER BY updated_at, id
                    LIMIT $1
                    "#,
                    limit,
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error("list parked jobs", e))?;
                rows.into_iter()
                    .map(|row| {
                        let checked_seq = from_bigint(row.wake_checked_seq, "checked sequence")?;
                        let job = JobRow {
                            document: row.document,
                            attempt: row.attempt,
                            updated_at: row.updated_at,
                            not_before: row.not_before,
                            lease_holder: row.lease_holder,
                            lease_acquired_at: row.lease_acquired_at,
                            lease_expires_at: row.lease_expires_at,
                            lease_heartbeat_at: row.lease_heartbeat_at,
                        }
                        .into_job()?;
                        Ok(Parked { job, checked_seq })
                    })
                    .collect()
            })
            .await
    }

    /// Claims in one statement. Under `READ COMMITTED`, a job that another
    /// claimer took after this statement's snapshot fails the recheck when
    /// locked, and `LIMIT 1` then yields nothing even if other jobs are
    /// claimable. So an empty claim is retried, with a fresh snapshot, while
    /// claimable jobs remain. A job whose row a fenced write holds in share
    /// mode is skipped too, until that write commits.
    async fn claim(
        &self,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Option<Claimed>, Error> {
        let lease = lease_interval(lease_for)?;
        self.db
            .bounded("claim job", async {
                for _ in 0..CLAIM_ATTEMPTS {
                    let row = sqlx::query_as!(
                        ClaimedRow,
                        r#"
                        WITH next AS (
                            SELECT id, state AS prior_state FROM jobs
                            WHERE (state = 'queued'
                                    AND (not_before IS NULL OR not_before <= autosys_now()))
                                OR (state = 'running'
                                    AND (lease_expires_at IS NULL OR lease_expires_at <= autosys_now()))
                            ORDER BY created_at, id
                            LIMIT 1
                            FOR UPDATE SKIP LOCKED
                        )
                        UPDATE jobs SET
                            document = jsonb_set(jobs.document, '{status}', '{"state": "running"}'),
                            attempt = jobs.attempt + 1,
                            updated_at = autosys_now(),
                            not_before = NULL,
                            lease_holder = $1,
                            lease_acquired_at = autosys_now(),
                            lease_expires_at = autosys_now() + $2::interval,
                            lease_heartbeat_at = autosys_now(),
                            lease_epoch = jobs.lease_epoch + 1
                        FROM next
                        WHERE jobs.id = next.id
                        RETURNING jobs.document AS "document!: Json<Job>",
                            jobs.attempt AS "attempt!", jobs.updated_at AS "updated_at!",
                            jobs.lease_holder, jobs.lease_acquired_at, jobs.lease_expires_at,
                            jobs.lease_heartbeat_at, jobs.lease_epoch AS "lease_epoch!",
                            next.prior_state = 'running' AS "interrupted!"
                        "#,
                        holder.as_str(),
                        lease,
                    )
                    .fetch_optional(&self.db.pool)
                    .await
                    .map_err(|e| db_error("claim job", e))?;
                    if let Some(row) = row {
                        let epoch = from_bigint(row.lease_epoch, "lease epoch")?;
                        let interrupted = row.interrupted;
                        let job = JobRow {
                            document: row.document,
                            attempt: row.attempt,
                            updated_at: row.updated_at,
                            not_before: None,
                            lease_holder: row.lease_holder,
                            lease_acquired_at: row.lease_acquired_at,
                            lease_expires_at: row.lease_expires_at,
                            lease_heartbeat_at: row.lease_heartbeat_at,
                        }
                        .into_job()?;
                        let fence = FenceToken::issue_for_claim(job.id.clone(), holder.clone(), epoch);
                        return Ok(Some(Claimed::new(job, fence, interrupted)));
                    }
                    let claimable = sqlx::query_scalar!(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM jobs
                            WHERE (state = 'queued'
                                    AND (not_before IS NULL OR not_before <= autosys_now()))
                                OR (state = 'running'
                                    AND (lease_expires_at IS NULL OR lease_expires_at <= autosys_now()))
                        ) AS "exists!"
                        "#
                    )
                    .fetch_one(&self.db.pool)
                    .await
                    .map_err(|e| db_error("claim job", e))?;
                    if !claimable {
                        return Ok(None);
                    }
                }
                Ok(None)
            })
            .await
    }

    async fn renew_lease(&self, fence: &FenceToken, lease_for: Duration) -> Result<Job, Error> {
        let lease = lease_interval(lease_for)?;
        let epoch = to_bigint(fence.epoch(), "lease epoch")?;
        self.db
            .bounded("renew lease", async {
                let row = sqlx::query_as!(
                    JobRow,
                    r#"
                    UPDATE jobs SET
                        lease_expires_at = autosys_now() + $3::interval,
                        lease_heartbeat_at = autosys_now(),
                        updated_at = autosys_now()
                    WHERE id = $1
                        AND state = 'running'
                        AND lease_holder = $2
                        AND lease_epoch = $4
                        AND lease_expires_at > autosys_now()
                    RETURNING document AS "document!: Json<Job>",
                        attempt AS "attempt!", updated_at AS "updated_at!", not_before,
                        lease_holder, lease_acquired_at, lease_expires_at, lease_heartbeat_at
                    "#,
                    fence.job_id().as_str(),
                    fence.holder().as_str(),
                    lease,
                    epoch,
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("renew lease", e))?;
                match row {
                    Some(row) => row.into_job(),
                    None => Err(refused(&self.db.pool, fence, "renew lease").await),
                }
            })
            .await
    }

    async fn save(&self, write: &JobWrite<'_>, fence: &FenceToken) -> Result<(), Error> {
        let job = write.job;
        if fence.job_id() != &job.id {
            return Err(invalid(format!(
                "the fence is for {}, not {}",
                fence.job_id().as_str(),
                job.id.as_str()
            )));
        }
        let state = job_state(&job.status);
        if write.retry_after.is_some() && state != JobState::Queued {
            return Err(invalid(format!(
                "a retry delay needs a queued job, not {state}"
            )));
        }
        let retry_after = write
            .retry_after
            .map(|delay| interval(delay, "retry delay"))
            .transpose()?;
        let (replace, record) = match write.continuation {
            RecordWrite::Keep => (false, None),
            RecordWrite::Replace(record) => (true, Some(record)),
        };
        let parked = matches!(state, JobState::Waiting | JobState::Suspended);
        let checked_seq = to_bigint(job.progress.last_event_seq, "last event seq")?;
        let epoch = to_bigint(fence.epoch(), "lease epoch")?;
        self.db
            .bounded("save job", async {
                // The lease stays only while the job keeps running; attempt
                // is never taken from the caller. `state` follows the
                // document's status.
                let saved = sqlx::query!(
                    r#"
                    UPDATE jobs SET
                        run_id = $3,
                        created_at = $4,
                        document = $5,
                        updated_at = autosys_now(),
                        continuation = CASE WHEN $8 THEN $9 ELSE continuation END,
                        not_before = autosys_now() + $10::interval,
                        wake_checked_seq = CASE WHEN $11 THEN $12 ELSE wake_checked_seq END,
                        lease_holder = CASE WHEN $6 THEN lease_holder END,
                        lease_acquired_at = CASE WHEN $6 THEN lease_acquired_at END,
                        lease_expires_at = CASE WHEN $6 THEN lease_expires_at END,
                        lease_heartbeat_at = CASE WHEN $6 THEN lease_heartbeat_at END
                    WHERE id = $1
                        AND state = 'running'
                        AND lease_holder = $2
                        AND lease_epoch = $7
                        AND lease_expires_at > autosys_now()
                    "#,
                    job.id.as_str(),
                    fence.holder().as_str(),
                    job.run_id.as_str(),
                    job.created_at.0,
                    Json(job) as _,
                    state == JobState::Running,
                    epoch,
                    replace,
                    record.map(Json) as _,
                    retry_after,
                    parked,
                    checked_seq,
                )
                .execute(&self.db.pool)
                .await
                .map_err(|e| db_error("save job", e))?;
                if saved.rows_affected() == 0 {
                    return Err(refused(&self.db.pool, fence, "save job").await);
                }
                Ok(())
            })
            .await
    }

    async fn wake(
        &self,
        id: &JobId,
        reason: &WakeReason,
        authority: Option<&WriteAuthority>,
    ) -> Result<bool, Error> {
        let fence = authorize(authority, StoreWrite::JobWake)?;
        self.db
            .bounded("wake job", async {
                let mut tx = self
                    .db
                    .pool
                    .begin()
                    .await
                    .map_err(|e| db_error("wake job", e))?;
                if let Some(fence) = fence {
                    verify_fence(&mut tx, fence, "wake job").await?;
                }
                let status = sqlx::query_scalar!(
                    r#"
                    SELECT document -> 'status' AS "status!: Json<JobStatus>"
                    FROM jobs
                    WHERE id = $1
                    FOR UPDATE
                    "#,
                    id.as_str(),
                )
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_error("wake job", e))?
                .ok_or_else(|| not_found("job", id.as_str()))?
                .0;
                let woken = match &status {
                    JobStatus::Waiting { checkpoint, .. }
                    | JobStatus::Suspended { checkpoint, .. }
                        if wakes(reason, wake_condition(&status)) =>
                    {
                        Some(JobStatus::Queued {
                            checkpoint: Some(checkpoint.clone()),
                        })
                    }
                    _ => None,
                };
                let woke = woken.is_some();
                if let Some(queued) = woken {
                    sqlx::query!(
                        r#"
                        UPDATE jobs
                        SET document = jsonb_set(document, '{status}', $2),
                            not_before = NULL,
                            updated_at = autosys_now()
                        WHERE id = $1
                        "#,
                        id.as_str(),
                        Json(&queued) as _,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| db_error("wake job", e))?;
                }
                tx.commit().await.map_err(|e| db_error("wake job", e))?;
                Ok(woke)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use contracts::ids::ApprovalId;

    use super::*;

    fn approval(id: &str) -> ApprovalId {
        id.parse().unwrap()
    }

    #[test]
    fn a_cancel_wakes_whatever_the_job_waits_for() {
        let waiting = WakeCondition::ApprovalAnswered(approval("appr-1"));
        assert!(wakes(&WakeReason::CancelRequested, Some(&waiting)));
        assert!(wakes(&WakeReason::CancelRequested, None));
    }

    #[test]
    fn an_answer_wakes_only_the_job_waiting_for_it() {
        let waiting = WakeCondition::ApprovalAnswered(approval("appr-1"));
        let answered = |id| WakeReason::ApprovalAnswered(approval(id));
        assert!(wakes(&answered("appr-1"), Some(&waiting)));
        assert!(!wakes(&answered("appr-2"), Some(&waiting)));
        assert!(!wakes(&answered("appr-1"), None));
    }

    #[test]
    fn leases_longer_than_an_interval_can_hold_are_rejected() {
        assert!(lease_interval(Duration::from_secs(30)).is_ok());
        assert!(lease_interval(Duration::MAX).is_err());
    }
}
