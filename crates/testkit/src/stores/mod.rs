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

use contracts::{Error, ErrorCode};

fn not_found(what: &str, id: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("{what} {id} does not exist"))
}

fn conflict(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Conflict, message)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}
