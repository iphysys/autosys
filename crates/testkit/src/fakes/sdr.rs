use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{SdrDocument, SdrVersionRecord};
use contracts::{Error, ErrorCode, SdrRepository, WriteAuthority};

/// How a [`FaultySdrRepository`] misbehaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Every faulted call fails with this code.
    Fail(ErrorCode),
    /// Every faulted call never finishes, so a timeout or a cancel is the
    /// only way out.
    Hang,
}

/// SDR storage that deliberately fails, to test how callers of `SdrManager`
/// treat a broken store. It stores nothing, so it has no contract suite.
///
/// Arch-check keeps most crates from naming the repository trait, so tests
/// outside `sdr` take the constructors [`faulty_sdr_storage`] and
/// [`faulty_sdr_storage_after_head`] and hand the result to `SdrManager::new`.
pub struct FaultySdrRepository {
    /// What `head` answers, if it is not faulted.
    head: Option<SdrVersion>,
    fault: Fault,
}

impl FaultySdrRepository {
    /// Every call, `head` included, faults.
    pub fn new(fault: Fault) -> Self {
        Self { head: None, fault }
    }

    /// `head` answers `version`; every other call faults. This is the path of
    /// a manager that finds the project and then fails to read its document.
    pub fn after_head(version: u64, fault: Fault) -> Self {
        Self {
            head: Some(SdrVersion(version)),
            fault,
        }
    }

    async fn faulted<T>(&self) -> Result<T, Error> {
        match self.fault {
            Fault::Fail(code) => Err(Error::new(code, "injected SDR storage fault")),
            Fault::Hang => std::future::pending().await,
        }
    }
}

#[async_trait]
impl SdrRepository for FaultySdrRepository {
    async fn head(&self, _project_id: &ProjectId) -> Result<Option<SdrVersion>, Error> {
        match &self.head {
            Some(version) => Ok(Some(version.clone())),
            None => self.faulted().await,
        }
    }

    async fn document(
        &self,
        _project_id: &ProjectId,
        _version: &SdrVersion,
    ) -> Result<SdrDocument, Error> {
        self.faulted().await
    }

    async fn version_record(
        &self,
        _project_id: &ProjectId,
        _version: &SdrVersion,
    ) -> Result<SdrVersionRecord, Error> {
        self.faulted().await
    }

    async fn append_version(
        &self,
        _document: &SdrDocument,
        _record: &SdrVersionRecord,
        _authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        self.faulted().await
    }
}

/// Storage whose every call faults, ready for `SdrManager::new`.
pub fn faulty_sdr_storage(fault: Fault) -> Arc<dyn SdrRepository> {
    Arc::new(FaultySdrRepository::new(fault))
}

/// Storage whose `head` answers `version` and whose other calls fault.
pub fn faulty_sdr_storage_after_head(version: u64, fault: Fault) -> Arc<dyn SdrRepository> {
    Arc::new(FaultySdrRepository::after_head(version, fault))
}
