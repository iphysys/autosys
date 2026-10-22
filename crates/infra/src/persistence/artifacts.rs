use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use contracts::ids::{ArtifactId, RunId};
use contracts::types::ArtifactMetadata;
use contracts::{ArtifactStore, Error, ErrorCode};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::Database;
use super::error::{conflict, corrupt, db_error, invalid, not_found, to_bigint};

/// Makes temporary file names unique within this process.
static TEMP_FILES: AtomicU64 = AtomicU64::new(0);

/// [`ArtifactStore`] with metadata in Postgres and content as files on the
/// artifact volume (ADR-0016). A content file is named by its sha256, so an
/// artifact ID never becomes part of a path, and identical content is stored
/// once.
pub struct PgArtifactStore {
    db: Database,
    root: PathBuf,
}

impl PgArtifactStore {
    /// `root` is the artifact volume; it is created on first write.
    pub fn new(db: Database, root: impl Into<PathBuf>) -> Self {
        Self {
            db,
            root: root.into(),
        }
    }

    async fn find(&self, id: &str) -> Result<Option<ArtifactMetadata>, Error> {
        let row = sqlx::query_scalar!(
            r#"SELECT metadata AS "metadata!: Json<ArtifactMetadata>" FROM artifacts WHERE id = $1"#,
            id,
        )
        .fetch_optional(&self.db.pool)
        .await
        .map_err(|e| db_error("get artifact metadata", e))?;
        Ok(row.map(|meta| meta.0))
    }

    /// Ok when `existing` is the artifact being stored again; Conflict
    /// otherwise.
    fn same_or_conflict(
        existing: &ArtifactMetadata,
        metadata: &ArtifactMetadata,
    ) -> Result<(), Error> {
        if existing == metadata {
            Ok(())
        } else {
            Err(conflict(format!(
                "artifact {} exists with different content",
                metadata.id.as_str()
            )))
        }
    }
}

/// `<root>/<first two hex digits>/<64 hex digits>`.
fn content_path(root: &Path, sha256: &str) -> Result<PathBuf, Error> {
    let hex = sha256
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .ok_or_else(|| invalid(format!("{sha256} is not a sha256 digest")))?;
    Ok(root.join(&hex[..2]).join(hex))
}

fn sha256_of(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

fn io_error(op: &str, err: io::Error) -> Error {
    Error::new(
        ErrorCode::Internal,
        format!("artifact volume error during {op}"),
    )
    .with_source(err)
}

/// Writes `content` to `path` unless it is already there. Writes a temporary
/// file, flushes it to disk, then renames it into place, so a reader never
/// sees a partial file. Same name means same content, so a concurrent writer
/// of the same file is harmless.
async fn write_content(path: &Path, content: &[u8]) -> Result<(), Error> {
    if fs::try_exists(path)
        .await
        .map_err(|e| io_error("write content", e))?
    {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| corrupt("content path has no directory"))?;
    fs::create_dir_all(dir)
        .await
        .map_err(|e| io_error("write content", e))?;
    let unique = TEMP_FILES.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".tmp-{}-{unique}", std::process::id()));
    let written = async {
        let mut file = fs::File::create(&temp).await?;
        file.write_all(content).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temp, path).await
    }
    .await;
    if let Err(e) = written {
        // Best effort: the temporary file is useless now.
        let _ = fs::remove_file(&temp).await;
        return Err(io_error("write content", e));
    }
    Ok(())
}

#[async_trait]
impl ArtifactStore for PgArtifactStore {
    async fn put(&self, metadata: &ArtifactMetadata, content: &[u8]) -> Result<(), Error> {
        let id = metadata.id.as_str();
        if metadata.size_bytes != content.len() as u64 {
            return Err(invalid(format!(
                "artifact {id} declares {} bytes but has {}",
                metadata.size_bytes,
                content.len()
            )));
        }
        if metadata.sha256.as_str() != sha256_of(content) {
            return Err(invalid(format!(
                "artifact {id} content does not match its sha256"
            )));
        }
        let path = content_path(&self.root, metadata.sha256.as_str())?;
        let size = to_bigint(metadata.size_bytes, "artifact size")?;
        self.db
            .bounded("put artifact", async {
                if let Some(existing) = self.find(id).await? {
                    return Self::same_or_conflict(&existing, metadata);
                }
                // Content first: metadata never points at a missing file.
                write_content(&path, content).await?;
                let inserted = sqlx::query!(
                    r#"
                    INSERT INTO artifacts (id, run_id, created_at, sha256, size_bytes, metadata)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    ON CONFLICT (id) DO NOTHING
                    "#,
                    id,
                    metadata.run_id.as_str(),
                    metadata.created_at.0,
                    metadata.sha256.as_str(),
                    size,
                    Json(metadata) as _,
                )
                .execute(&self.db.pool)
                .await
                .map_err(|e| db_error("put artifact", e))?;
                if inserted.rows_affected() == 1 {
                    return Ok(());
                }
                // Another writer stored this ID between the check and the insert.
                match self.find(id).await? {
                    Some(existing) => Self::same_or_conflict(&existing, metadata),
                    None => Err(corrupt(format!(
                        "artifact {id} vanished while being stored"
                    ))),
                }
            })
            .await
    }

    async fn metadata(&self, id: &ArtifactId) -> Result<ArtifactMetadata, Error> {
        self.db
            .bounded("get artifact metadata", async {
                self.find(id.as_str())
                    .await?
                    .ok_or_else(|| not_found("artifact", id.as_str()))
            })
            .await
    }

    /// Checks the content against its metadata, so a damaged file is
    /// reported instead of returned.
    async fn content(&self, id: &ArtifactId) -> Result<Vec<u8>, Error> {
        self.db
            .bounded("get artifact content", async {
                let metadata = self
                    .find(id.as_str())
                    .await?
                    .ok_or_else(|| not_found("artifact", id.as_str()))?;
                let path = content_path(&self.root, metadata.sha256.as_str())?;
                let content = fs::read(&path)
                    .await
                    .map_err(|e| io_error("read content", e))?;
                if content.len() as u64 != metadata.size_bytes
                    || sha256_of(&content) != metadata.sha256.as_str()
                {
                    return Err(corrupt(format!(
                        "content of artifact {} does not match its metadata",
                        id.as_str()
                    )));
                }
                Ok(content)
            })
            .await
    }

    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<ArtifactMetadata>, Error> {
        self.db
            .bounded("list artifacts", async {
                let rows = sqlx::query_scalar!(
                    r#"
                    SELECT metadata AS "metadata!: Json<ArtifactMetadata>"
                    FROM artifacts
                    WHERE run_id = $1
                    ORDER BY created_at, id
                    "#,
                    run_id.as_str(),
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error("list artifacts", e))?;
                Ok(rows.into_iter().map(|row| row.0).collect())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_paths_come_only_from_the_digest() {
        let root = Path::new("volume");
        let hex = "ab".repeat(32);
        let path = content_path(root, &format!("sha256:{hex}")).unwrap();
        assert_eq!(path, root.join("ab").join(&hex));
        let upper = format!("sha256:{}", "AB".repeat(32));
        for bad in [
            "sha256:../../etc/passwd",
            "md5:abc",
            "sha256:",
            &upper,
            "ab",
        ] {
            assert!(content_path(root, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn digests_match_the_schema_format() {
        assert_eq!(
            sha256_of(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
