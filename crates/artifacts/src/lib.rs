//! Artifacts and the run report (blueprint sections 2.7 and 18.1).
//!
//! [`Metadata`] describes an artifact's content and provenance: format,
//! producing tool and version, and the SDR version it was made from.
//! [`ReportAssembler`] builds the run report on demand from durable records
//! only: the job and its continuation, the session log, the SDR, the evidence
//! store and the artifact store (ADR-0033). It writes nothing, so the same
//! stopped run always gives the same report.

mod error;
mod metadata;
mod reports;

pub use error::ArtifactsError;
pub use metadata::{ArtifactProvenance, Metadata, report_row};
pub use reports::{ASSEMBLER_VERSION, ReportAssembler, ReportSources};
