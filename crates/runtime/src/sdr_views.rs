//! Committed SDR versions for clients (ADR-0035): the document, its record
//! and the evidence the record cites, read through the SDR manager behind
//! the contract the API calls.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::{ElementKind, SdrSlice, SdrVersionView, SdrVersionViewSchemaVersion};
use contracts::{Error, ErrorCode, EvidenceStore, ExecutionContext, SdrViews};
use sdr::SdrManager;

/// Serves each committed version as the harness stored it. Reads only.
pub struct Views {
    sdr: Arc<SdrManager>,
    evidence: Arc<dyn EvidenceStore>,
    /// Bounds each read.
    io_timeout: Duration,
}

impl Views {
    pub fn new(
        sdr: Arc<SdrManager>,
        evidence: Arc<dyn EvidenceStore>,
        io_timeout: Duration,
    ) -> Self {
        Self {
            sdr,
            evidence,
            io_timeout,
        }
    }

    async fn bounded<T>(
        &self,
        what: &str,
        read: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        tokio::time::timeout(self.io_timeout, read)
            .await
            .unwrap_or_else(|_| {
                Err(Error::new(
                    ErrorCode::Timeout,
                    format!("{what} took longer than {:?}", self.io_timeout),
                ))
            })
    }
}

#[async_trait]
impl SdrViews for Views {
    async fn version(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionView, Error> {
        ctx.ensure_not_cancelled()?;
        let everything = SdrSlice {
            kinds: vec![
                ElementKind::Requirement,
                ElementKind::Parameter,
                ElementKind::Decision,
                ElementKind::Assumption,
            ],
            ids: Vec::new(),
        };
        let record = self
            .bounded(
                "reading the version record",
                self.sdr.version_record(project_id, version, ctx),
            )
            .await?;
        let document = self
            .bounded(
                "reading the design",
                self.sdr.read(project_id, &everything, version, ctx),
            )
            .await?;
        let mut evidence = Vec::with_capacity(record.evidence_ids.len());
        for id in &record.evidence_ids {
            evidence.push(
                self.bounded("reading evidence", self.evidence.get(id))
                    .await?,
            );
        }
        Ok(SdrVersionView {
            schema_version: SdrVersionViewSchemaVersion::X01,
            document,
            record,
            evidence,
        })
    }
}
