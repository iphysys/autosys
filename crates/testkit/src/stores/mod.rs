//! In-memory implementations of the store traits. Each passes the matching
//! suite in [`crate::contract`].

mod artifact;
mod event;
mod evidence;
mod job;
mod sdr;

pub use artifact::InMemoryArtifactStore;
pub use event::InMemoryEventLog;
pub use evidence::InMemoryEvidenceStore;
pub use job::InMemoryJobStore;
pub use sdr::InMemorySdrRepository;

use std::sync::Arc;

use contracts::{Error, ErrorCode, SdrRepository};

/// In-memory SDR storage for tests outside `crates/sdr` that need an
/// `SdrManager`. Hand it straight to `SdrManager::new`: arch-check keeps
/// other crates from naming the repository trait, so they cannot use it
/// any other way (blueprint section 7.2).
pub fn sdr_storage() -> Arc<dyn SdrRepository> {
    Arc::new(InMemorySdrRepository::new())
}

fn not_found(what: &str, id: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("{what} {id} does not exist"))
}

fn conflict(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Conflict, message)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}
