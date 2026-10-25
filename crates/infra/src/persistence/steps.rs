use async_trait::async_trait;
use chrono::{DateTime, Utc};
use contracts::ids::Identifier;
use contracts::types::{StepOutcome, StepRecord, StepRecordSchemaVersion, Timestamp};
use contracts::{Error, NewStep, StepStart, StepStore, StoreWrite, WriteAuthority, authorize};
use sqlx::postgres::PgExecutor;
use sqlx::types::Json;

use super::Database;
use super::error::{conflict, corrupt, db_error, not_found};
use super::fence::verify_fence;

/// [`StepStore`] on Postgres. `begin` is an insert that does nothing when
/// the key exists, so concurrent callers in any number of processes start a
/// step at most once. Each write is one transaction that checks its fence
/// first. Times come from the database clock.
pub struct PgStepStore {
    db: Database,
}

impl PgStepStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

async fn find<'e>(executor: impl PgExecutor<'e>, key: &str) -> Result<Option<StepRecord>, Error> {
    let row = sqlx::query_as!(
        StepRow,
        r#"
            SELECT idempotency_key, run_id, step_id, tool, request_sha256, started_at,
                   completed_at, outcome AS "outcome: Json<StepOutcome>"
            FROM step_records
            WHERE idempotency_key = $1
            "#,
        key,
    )
    .fetch_optional(executor)
    .await
    .map_err(|e| db_error("get step", e))?;
    row.map(StepRow::into_record).transpose()
}

struct StepRow {
    idempotency_key: String,
    run_id: String,
    step_id: String,
    tool: String,
    request_sha256: String,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    outcome: Option<Json<StepOutcome>>,
}

impl StepRow {
    fn into_record(self) -> Result<StepRecord, Error> {
        let key = self.idempotency_key;
        let bad = |field: &str| corrupt(format!("step {key} has an invalid {field}"));
        Ok(StepRecord {
            schema_version: StepRecordSchemaVersion::X01,
            run_id: self.run_id.parse().map_err(|_| bad("run_id"))?,
            step_id: self.step_id.parse().map_err(|_| bad("step_id"))?,
            tool: self.tool.parse().map_err(|_| bad("tool"))?,
            request_sha256: self
                .request_sha256
                .parse()
                .map_err(|_| bad("request_sha256"))?,
            started_at: Timestamp(self.started_at),
            completed_at: self.completed_at.map(Timestamp),
            outcome: self.outcome.map(|o| o.0),
            idempotency_key: key.parse().map_err(|_| bad("idempotency_key"))?,
        })
    }
}

fn same_request(record: &StepRecord, step: &NewStep) -> bool {
    record.run_id == step.run_id
        && record.step_id == step.step_id
        && record.tool == step.tool
        && record.request_sha256 == step.request_sha256
}

#[async_trait]
impl StepStore for PgStepStore {
    async fn begin(
        &self,
        step: &NewStep,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepStart, Error> {
        let fence = authorize(authority, StoreWrite::StepBegin)?;
        let key = step.idempotency_key.as_str();
        self.db
            .bounded("begin step", async {
                let mut tx = self
                    .db
                    .pool
                    .begin()
                    .await
                    .map_err(|e| db_error("begin step", e))?;
                if let Some(fence) = fence {
                    verify_fence(&mut tx, fence, "begin step").await?;
                }
                let inserted = sqlx::query_as!(
                    StepRow,
                    r#"
                    INSERT INTO step_records
                        (idempotency_key, run_id, step_id, tool, request_sha256, started_at)
                    VALUES ($1, $2, $3, $4, $5, autosys_now())
                    ON CONFLICT (idempotency_key) DO NOTHING
                    RETURNING idempotency_key, run_id, step_id, tool, request_sha256,
                              started_at, completed_at,
                              outcome AS "outcome: Json<StepOutcome>"
                    "#,
                    key,
                    step.run_id.as_str(),
                    step.step_id.as_str(),
                    step.tool.as_str(),
                    step.request_sha256.as_str(),
                )
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_error("begin step", e))?;
                let start = match inserted {
                    Some(row) => StepStart::Started(row.into_record()?),
                    None => {
                        let existing = find(&mut *tx, key)
                            .await?
                            .ok_or_else(|| corrupt(format!("step {key} vanished during begin")))?;
                        if !same_request(&existing, step) {
                            return Err(conflict(format!(
                                "idempotency key {key} was used for a different request"
                            )));
                        }
                        StepStart::Existing(existing)
                    }
                };
                tx.commit().await.map_err(|e| db_error("begin step", e))?;
                Ok(start)
            })
            .await
    }

    async fn complete(
        &self,
        key: &Identifier,
        outcome: &StepOutcome,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepRecord, Error> {
        let fence = authorize(authority, StoreWrite::StepComplete)?;
        let key = key.as_str();
        self.db
            .bounded("complete step", async {
                let mut tx = self
                    .db
                    .pool
                    .begin()
                    .await
                    .map_err(|e| db_error("complete step", e))?;
                if let Some(fence) = fence {
                    verify_fence(&mut tx, fence, "complete step").await?;
                }
                let updated = sqlx::query_as!(
                    StepRow,
                    r#"
                    UPDATE step_records
                    SET completed_at = autosys_now(), outcome = $2
                    WHERE idempotency_key = $1 AND outcome IS NULL
                    RETURNING idempotency_key, run_id, step_id, tool, request_sha256,
                              started_at, completed_at,
                              outcome AS "outcome: Json<StepOutcome>"
                    "#,
                    key,
                    Json(outcome) as _,
                )
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_error("complete step", e))?;
                let record = match updated {
                    Some(row) => row.into_record()?,
                    None => {
                        let existing = find(&mut *tx, key)
                            .await?
                            .ok_or_else(|| not_found("step", key))?;
                        if existing.outcome.as_ref() != Some(outcome) {
                            return Err(conflict(format!(
                                "step {key} already completed with a different outcome"
                            )));
                        }
                        existing
                    }
                };
                tx.commit()
                    .await
                    .map_err(|e| db_error("complete step", e))?;
                Ok(record)
            })
            .await
    }

    async fn get(&self, key: &Identifier) -> Result<StepRecord, Error> {
        self.db
            .bounded("get step", async {
                find(&self.db.pool, key.as_str())
                    .await?
                    .ok_or_else(|| not_found("step", key.as_str()))
            })
            .await
    }
}
