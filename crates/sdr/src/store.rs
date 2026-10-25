//! Access to SDR storage. Crate-private: nothing outside `SdrManager` reaches
//! the repository (blueprint section 7.2).

use std::sync::Arc;

use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{SdrDocument, SdrVersionRecord};
use contracts::{Error, ExecutionContext, SdrRepository};

pub(crate) struct Store {
    repository: Arc<dyn SdrRepository>,
}

impl Store {
    pub(crate) fn new(repository: Arc<dyn SdrRepository>) -> Self {
        Self { repository }
    }

    pub(crate) async fn head(
        &self,
        project_id: &ProjectId,
        ctx: &ExecutionContext,
    ) -> Result<Option<SdrVersion>, Error> {
        ctx.ensure_not_cancelled()?;
        self.repository.head(project_id).await
    }

    pub(crate) async fn document(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrDocument, Error> {
        ctx.ensure_not_cancelled()?;
        self.repository.document(project_id, version).await
    }

    pub(crate) async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionRecord, Error> {
        ctx.ensure_not_cancelled()?;
        self.repository.version_record(project_id, version).await
    }

    /// Conflict if another version was appended since the base was read;
    /// LeaseLost if `ctx`'s write authority does not permit the append.
    pub(crate) async fn append(
        &self,
        document: &SdrDocument,
        record: &SdrVersionRecord,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        ctx.ensure_not_cancelled()?;
        self.repository
            .append_version(document, record, ctx.write_authority())
            .await
    }
}
