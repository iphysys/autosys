use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::{ArtifactId, RunId};
use contracts::types::ArtifactMetadata;
use contracts::{ArtifactStore, Error};

use super::{conflict, invalid, not_found};
use crate::builders::sha256_of;

#[derive(Default)]
pub struct InMemoryArtifactStore {
    artifacts: Mutex<BTreeMap<String, (ArtifactMetadata, Vec<u8>)>>,
}

impl InMemoryArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ArtifactStore for InMemoryArtifactStore {
    async fn put(&self, metadata: &ArtifactMetadata, content: &[u8]) -> Result<(), Error> {
        if metadata.size_bytes != content.len() as u64 {
            return Err(invalid(format!(
                "artifact {} declares {} bytes but has {}",
                metadata.id.as_str(),
                metadata.size_bytes,
                content.len()
            )));
        }
        if metadata.sha256.as_str() != sha256_of(content) {
            return Err(invalid(format!(
                "artifact {} content does not match its sha256",
                metadata.id.as_str()
            )));
        }
        let mut artifacts = self.artifacts.lock().unwrap();
        match artifacts.get(metadata.id.as_str()) {
            Some((existing, bytes)) if existing == metadata && bytes == content => Ok(()),
            Some(_) => Err(conflict(format!(
                "artifact {} exists with different content",
                metadata.id.as_str()
            ))),
            None => {
                artifacts.insert(
                    metadata.id.as_str().to_owned(),
                    (metadata.clone(), content.to_vec()),
                );
                Ok(())
            }
        }
    }

    async fn metadata(&self, id: &ArtifactId) -> Result<ArtifactMetadata, Error> {
        self.artifacts
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|(meta, _)| meta.clone())
            .ok_or_else(|| not_found("artifact", id.as_str()))
    }

    async fn content(&self, id: &ArtifactId) -> Result<Vec<u8>, Error> {
        self.artifacts
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| not_found("artifact", id.as_str()))
    }

    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<ArtifactMetadata>, Error> {
        let mut found: Vec<ArtifactMetadata> = self
            .artifacts
            .lock()
            .unwrap()
            .values()
            .filter(|(meta, _)| &meta.run_id == run_id)
            .map(|(meta, _)| meta.clone())
            .collect();
        found.sort_by(|a, b| (a.created_at.0, a.id.as_str()).cmp(&(b.created_at.0, b.id.as_str())));
        Ok(found)
    }
}
