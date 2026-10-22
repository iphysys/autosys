use async_trait::async_trait;
use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{SdrDocument, SdrVersionRecord};
use contracts::{Error, SdrRepository};
use sqlx::types::Json;

use super::Database;
use super::error::{
    conflict, db_error, from_bigint, invalid, is_unique_violation, not_found, to_bigint,
};

/// [`SdrRepository`] on Postgres. Versions are linear per project; the
/// primary key turns a concurrent append of the same version into a
/// conflict. Only `SdrManager` receives this repository (blueprint 7.2).
pub struct PgSdrRepository {
    db: Database,
}

impl PgSdrRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    async fn current_head(&self, project_id: &str) -> Result<Option<u64>, Error> {
        let head = sqlx::query_scalar!(
            "SELECT max(version) FROM sdr_versions WHERE project_id = $1",
            project_id,
        )
        .fetch_one(&self.db.pool)
        .await
        .map_err(|e| db_error("read SDR head", e))?;
        head.map(|v| from_bigint(v, "SDR version")).transpose()
    }
}

fn missing(project_id: &ProjectId, version: &SdrVersion) -> Error {
    not_found(
        "SDR version",
        &format!("{}@{}", project_id.as_str(), version.0),
    )
}

#[async_trait]
impl SdrRepository for PgSdrRepository {
    async fn head(&self, project_id: &ProjectId) -> Result<Option<SdrVersion>, Error> {
        self.db
            .bounded("read SDR head", async {
                Ok(self
                    .current_head(project_id.as_str())
                    .await?
                    .map(SdrVersion))
            })
            .await
    }

    async fn document(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrDocument, Error> {
        let Ok(number) = i64::try_from(version.0) else {
            return Err(missing(project_id, version));
        };
        self.db
            .bounded("read SDR document", async {
                let row = sqlx::query_scalar!(
                    r#"
                    SELECT document AS "document!: Json<SdrDocument>"
                    FROM sdr_versions
                    WHERE project_id = $1 AND version = $2
                    "#,
                    project_id.as_str(),
                    number,
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("read SDR document", e))?;
                row.map(|doc| doc.0)
                    .ok_or_else(|| missing(project_id, version))
            })
            .await
    }

    async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrVersionRecord, Error> {
        let Ok(number) = i64::try_from(version.0) else {
            return Err(missing(project_id, version));
        };
        self.db
            .bounded("read SDR version record", async {
                let row = sqlx::query_scalar!(
                    r#"
                    SELECT record AS "record!: Json<SdrVersionRecord>"
                    FROM sdr_versions
                    WHERE project_id = $1 AND version = $2
                    "#,
                    project_id.as_str(),
                    number,
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("read SDR version record", e))?;
                row.map(|record| record.0)
                    .ok_or_else(|| missing(project_id, version))
            })
            .await
    }

    async fn append_version(
        &self,
        document: &SdrDocument,
        record: &SdrVersionRecord,
    ) -> Result<(), Error> {
        if document.project_id != record.project_id
            || document.version != record.version
            || document.parent_version != record.parent_version
        {
            return Err(invalid(
                "document and version record disagree on project, version or parent",
            ));
        }
        let version = document.version.0;
        let parent = document.parent_version.as_ref().map(|p| p.0);
        let project = document.project_id.as_str();
        self.db
            .bounded("append SDR version", async {
                let head = self.current_head(project).await?;
                let expected = head.map_or(0, |h| h + 1);
                if version != expected || parent != head {
                    return Err(conflict(format!(
                        "version {version} with parent {parent:?} does not follow head {head:?}"
                    )));
                }
                let stored = sqlx::query!(
                    r#"
                    INSERT INTO sdr_versions (project_id, version, parent_version, document, record)
                    VALUES ($1, $2, $3, $4, $5)
                    "#,
                    project,
                    to_bigint(version, "SDR version")?,
                    parent.map(|p| to_bigint(p, "SDR version")).transpose()?,
                    Json(document) as _,
                    Json(record) as _,
                )
                .execute(&self.db.pool)
                .await;
                match stored {
                    Ok(_) => Ok(()),
                    // Another writer appended this version after the head was read.
                    Err(e) if is_unique_violation(&e) => Err(conflict(format!(
                        "version {version} of {project} was appended concurrently"
                    ))),
                    Err(e) => Err(db_error("append SDR version", e)),
                }
            })
            .await
    }
}
