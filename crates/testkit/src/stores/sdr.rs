use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{SdrDocument, SdrVersionRecord};
use contracts::{Error, SdrRepository};

use super::{conflict, invalid, not_found};

/// Versions per project, in order: index i holds version i.
#[derive(Default)]
pub struct InMemorySdrRepository {
    projects: Mutex<HashMap<String, Vec<(SdrDocument, SdrVersionRecord)>>>,
}

impl InMemorySdrRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry<T>(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        pick: impl Fn(&(SdrDocument, SdrVersionRecord)) -> T,
    ) -> Result<T, Error> {
        let projects = self.projects.lock().unwrap();
        usize::try_from(version.0)
            .ok()
            .and_then(|i| projects.get(project_id.as_str())?.get(i))
            .map(pick)
            .ok_or_else(|| {
                not_found(
                    "SDR version",
                    &format!("{}@{}", project_id.as_str(), version.0),
                )
            })
    }
}

#[async_trait]
impl SdrRepository for InMemorySdrRepository {
    async fn head(&self, project_id: &ProjectId) -> Result<Option<SdrVersion>, Error> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .get(project_id.as_str())
            .and_then(|versions| versions.len().checked_sub(1))
            .map(|last| SdrVersion(last as u64)))
    }

    async fn document(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrDocument, Error> {
        self.entry(project_id, version, |(doc, _)| doc.clone())
    }

    async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrVersionRecord, Error> {
        self.entry(project_id, version, |(_, record)| record.clone())
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
        let mut projects = self.projects.lock().unwrap();
        let versions = projects
            .entry(document.project_id.as_str().to_owned())
            .or_default();
        let head = versions.len().checked_sub(1).map(|h| h as u64);
        let expected = head.map_or(0, |h| h + 1);
        let parent = document.parent_version.as_ref().map(|p| p.0);
        if document.version.0 != expected || parent != head {
            return Err(conflict(format!(
                "version {} with parent {:?} does not follow head {:?}",
                document.version.0, parent, head
            )));
        }
        versions.push((document.clone(), record.clone()));
        Ok(())
    }
}
