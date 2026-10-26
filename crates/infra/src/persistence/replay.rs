use std::collections::BTreeSet;

use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::replay::{ReplayBlob, blob_hashes, verify};
use contracts::types::{ModelCallRecord, Sha256};
use contracts::{Error, ErrorCode, ReplayStore};
use sqlx::types::Json;

use super::Database;
use super::error::{conflict, db_error, not_found};

/// [`ReplayStore`] on Postgres. A record and its new blobs are stored in one
/// transaction; blobs already stored are kept as they are.
pub struct PgReplayStore {
    db: Database,
}

impl PgReplayStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ReplayStore for PgReplayStore {
    async fn put_call(&self, record: &ModelCallRecord, blobs: &[ReplayBlob]) -> Result<(), Error> {
        let op = "put model call";
        self.db
            .bounded(op, async {
                let given: BTreeSet<&str> = blobs.iter().map(|b| b.sha256.as_str()).collect();
                let elsewhere: Vec<String> = blob_hashes(record)
                    .into_iter()
                    .map(|h| h.as_str())
                    .filter(|h| !given.contains(h))
                    .map(str::to_owned)
                    .collect();
                let mut tx = self.db.pool.begin().await.map_err(|e| db_error(op, e))?;
                let stored: BTreeSet<String> = sqlx::query_scalar!(
                    "SELECT sha256 FROM replay_blobs WHERE sha256 = ANY($1)",
                    &elsewhere,
                )
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| db_error(op, e))?
                .into_iter()
                .collect();
                verify(record, blobs, |hash| stored.contains(hash.as_str()))?;

                let hashes: Vec<String> =
                    blobs.iter().map(|b| b.sha256.as_str().to_owned()).collect();
                let contents: Vec<String> = blobs.iter().map(|b| b.content.to_string()).collect();
                sqlx::query!(
                    r#"
                    INSERT INTO replay_blobs (sha256, content)
                    SELECT * FROM UNNEST($1::text[], $2::text[])
                    ON CONFLICT (sha256) DO NOTHING
                    "#,
                    &hashes,
                    &contents,
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| db_error(op, e))?;

                let seq = seq_column(record.context_seq.get())?;
                let inserted = sqlx::query!(
                    r#"
                    INSERT INTO replay_calls (run_id, context_seq, record)
                    VALUES ($1, $2, $3)
                    ON CONFLICT (run_id, context_seq) DO NOTHING
                    "#,
                    record.run_id.as_str(),
                    seq,
                    Json(record) as _,
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| db_error(op, e))?;
                if inserted.rows_affected() == 0 {
                    let existing = sqlx::query_scalar!(
                        r#"
                        SELECT record AS "record!: Json<ModelCallRecord>"
                        FROM replay_calls WHERE run_id = $1 AND context_seq = $2
                        "#,
                        record.run_id.as_str(),
                        seq,
                    )
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| db_error(op, e))?;
                    if existing.0 != *record {
                        return Err(conflict(format!(
                            "run {} already has a different call at context seq {seq}",
                            record.run_id.as_str()
                        )));
                    }
                }
                tx.commit().await.map_err(|e| db_error(op, e))
            })
            .await
    }

    async fn call(&self, run_id: &RunId, context_seq: u64) -> Result<ModelCallRecord, Error> {
        let op = "get model call";
        self.db
            .bounded(op, async {
                let row = sqlx::query_scalar!(
                    r#"
                    SELECT record AS "record!: Json<ModelCallRecord>"
                    FROM replay_calls WHERE run_id = $1 AND context_seq = $2
                    "#,
                    run_id.as_str(),
                    seq_column(context_seq)?,
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error(op, e))?;
                row.map(|record| record.0).ok_or_else(|| {
                    not_found("model call", &format!("{}@{context_seq}", run_id.as_str()))
                })
            })
            .await
    }

    async fn calls_for_run(&self, run_id: &RunId) -> Result<Vec<ModelCallRecord>, Error> {
        let op = "list model calls";
        self.db
            .bounded(op, async {
                let rows = sqlx::query_scalar!(
                    r#"
                    SELECT record AS "record!: Json<ModelCallRecord>"
                    FROM replay_calls WHERE run_id = $1 ORDER BY context_seq
                    "#,
                    run_id.as_str(),
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error(op, e))?;
                Ok(rows.into_iter().map(|record| record.0).collect())
            })
            .await
    }

    async fn blob(&self, sha256: &Sha256) -> Result<ReplayBlob, Error> {
        let op = "get replay blob";
        self.db
            .bounded(op, async {
                let content = sqlx::query_scalar!(
                    "SELECT content FROM replay_blobs WHERE sha256 = $1",
                    sha256.as_str(),
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error(op, e))?
                .ok_or_else(|| not_found("replay blob", sha256.as_str()))?;
                let content = serde_json::from_str(&content).map_err(|e| {
                    Error::new(
                        ErrorCode::Internal,
                        format!("replay blob {} is not JSON: {e}", sha256.as_str()),
                    )
                })?;
                Ok(ReplayBlob {
                    sha256: sha256.clone(),
                    content,
                })
            })
            .await
    }
}

/// `seq` as the `bigint` column holds it.
fn seq_column(seq: u64) -> Result<i64, Error> {
    i64::try_from(seq).map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("context seq {seq} is out of range"),
        )
    })
}
