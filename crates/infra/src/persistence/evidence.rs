use async_trait::async_trait;
use contracts::ids::{EvidenceId, RunId};
use contracts::types::Evidence;
use contracts::{Error, EvidenceStore};
use sqlx::types::Json;

use super::Database;
use super::error::{conflict, db_error, not_found};

/// [`EvidenceStore`] on Postgres. Evidence is immutable once stored.
pub struct PgEvidenceStore {
    db: Database,
}

impl PgEvidenceStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    async fn find(&self, id: &str) -> Result<Option<Evidence>, Error> {
        let row = sqlx::query_scalar!(
            r#"SELECT document AS "document!: Json<Evidence>" FROM evidence WHERE id = $1"#,
            id,
        )
        .fetch_optional(&self.db.pool)
        .await
        .map_err(|e| db_error("get evidence", e))?;
        Ok(row.map(|doc| doc.0))
    }
}

#[async_trait]
impl EvidenceStore for PgEvidenceStore {
    async fn put(&self, evidence: &Evidence) -> Result<(), Error> {
        let id = evidence.id.as_str();
        self.db
            .bounded("put evidence", async {
                let inserted = sqlx::query!(
                    r#"
                    INSERT INTO evidence (id, run_id, created_at, document)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (id) DO NOTHING
                    "#,
                    id,
                    evidence.run_id.as_str(),
                    evidence.created_at.0,
                    Json(evidence) as _,
                )
                .execute(&self.db.pool)
                .await
                .map_err(|e| db_error("put evidence", e))?;
                if inserted.rows_affected() == 1 {
                    return Ok(());
                }
                match self.find(id).await? {
                    Some(existing) if existing == *evidence => Ok(()),
                    _ => Err(conflict(format!(
                        "evidence {id} exists with different content"
                    ))),
                }
            })
            .await
    }

    async fn get(&self, id: &EvidenceId) -> Result<Evidence, Error> {
        self.db
            .bounded("get evidence", async {
                self.find(id.as_str())
                    .await?
                    .ok_or_else(|| not_found("evidence", id.as_str()))
            })
            .await
    }

    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<Evidence>, Error> {
        self.db
            .bounded("list evidence", async {
                let rows = sqlx::query_scalar!(
                    r#"
                    SELECT document AS "document!: Json<Evidence>"
                    FROM evidence
                    WHERE run_id = $1
                    ORDER BY created_at, id
                    "#,
                    run_id.as_str(),
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error("list evidence", e))?;
                Ok(rows.into_iter().map(|row| row.0).collect())
            })
            .await
    }
}
