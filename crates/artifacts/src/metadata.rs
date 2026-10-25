//! Artifact metadata (blueprint section 2.7): what an artifact is, what
//! produced it and from which SDR version. The bytes live in the artifact
//! store; the metadata names them by sha256.

use contracts::SchemaSet;
use contracts::ids::{ArtifactId, EvidenceId, Identifier, ProjectId, RunId, SdrVersion, StepId};
use contracts::types::{
    ArtifactMetadata, ArtifactMetadataMediaType, ArtifactMetadataSchemaVersion, NonEmptyString,
    ReportArtifact, Sha256, Timestamp, ToolName,
};
use sha2::Digest;

use crate::error::ArtifactsError;

const ARTIFACT: &str = "artifacts/artifact.schema.json";

/// What produced an artifact, and from what. [`Metadata::describe`] adds the
/// content's hash and size.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactProvenance {
    pub id: ArtifactId,
    pub run_id: RunId,
    pub project_id: ProjectId,
    /// Short format name, for example `step`.
    pub format: Identifier,
    pub media_type: ArtifactMetadataMediaType,
    pub source_sdr_version: SdrVersion,
    pub tool: ToolName,
    pub tool_version: NonEmptyString,
    /// The tool call that produced it; its step record holds the generation
    /// parameters.
    pub step_id: StepId,
    pub evidence_ids: Vec<EvidenceId>,
    pub created_at: Timestamp,
}

/// Describes artifacts, checked against `artifact.schema.json`.
pub struct Metadata {
    schemas: SchemaSet,
}

impl Metadata {
    pub fn new() -> Result<Self, ArtifactsError> {
        let schemas = SchemaSet::compile(&[ARTIFACT])
            .map_err(|e| ArtifactsError::Internal(format!("the artifact schema: {e}")))?;
        Ok(Self { schemas })
    }

    /// The metadata of `content` with `provenance`. InvalidMetadata if the
    /// result breaks the schema.
    pub fn describe(
        &self,
        content: &[u8],
        provenance: ArtifactProvenance,
    ) -> Result<ArtifactMetadata, ArtifactsError> {
        let metadata = ArtifactMetadata {
            schema_version: ArtifactMetadataSchemaVersion::X01,
            id: provenance.id,
            run_id: provenance.run_id,
            project_id: provenance.project_id,
            format: provenance.format,
            media_type: provenance.media_type,
            sha256: sha256_of(content)?,
            size_bytes: u64::try_from(content.len())
                .map_err(|_| ArtifactsError::Internal("an artifact's size overflows".into()))?,
            source_sdr_version: provenance.source_sdr_version,
            tool: provenance.tool,
            tool_version: provenance.tool_version,
            step_id: provenance.step_id,
            evidence_ids: provenance.evidence_ids,
            created_at: provenance.created_at,
        };
        let problems = self
            .schemas
            .errors_of(ARTIFACT, &metadata)
            .map_err(|e| ArtifactsError::Internal(format!("validating artifact metadata: {e}")))?;
        if problems.is_empty() {
            Ok(metadata)
        } else {
            Err(ArtifactsError::InvalidMetadata { problems })
        }
    }
}

/// The run report's row for an artifact (blueprint section 18.1).
pub fn report_row(metadata: &ArtifactMetadata) -> ReportArtifact {
    ReportArtifact {
        artifact_id: metadata.id.clone(),
        format: metadata.format.clone(),
        source_sdr_version: metadata.source_sdr_version.clone(),
        tool: metadata.tool.clone(),
        tool_version: metadata.tool_version.clone(),
    }
}

fn sha256_of(content: &[u8]) -> Result<Sha256, ArtifactsError> {
    let hex: String = sha2::Sha256::digest(content)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
        .parse()
        .map_err(|_| ArtifactsError::Internal("a content hash is not a valid sha256".into()))
}

#[cfg(test)]
mod tests {
    use testkit::builders::id;

    use super::*;

    fn provenance() -> ArtifactProvenance {
        ArtifactProvenance {
            id: id("art-1"),
            run_id: id("run-1"),
            project_id: id("p-1"),
            format: id("step"),
            media_type: id("model/step"),
            source_sdr_version: SdrVersion(2),
            tool: id("create_plate"),
            tool_version: id("0.1.0"),
            step_id: id("step-3.1"),
            evidence_ids: vec![id("ev-1")],
            created_at: id("2026-10-26T09:00:00Z"),
        }
    }

    #[test]
    fn describing_hashes_and_sizes_the_content() {
        let metadata = Metadata::new()
            .expect("schemas")
            .describe(b"abc", provenance())
            .expect("valid metadata");
        assert_eq!(metadata.size_bytes, 3);
        assert_eq!(
            metadata.sha256.as_str(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let row = report_row(&metadata);
        assert_eq!(row.artifact_id.as_str(), "art-1");
        assert_eq!(row.source_sdr_version.0, 2);
        assert_eq!(row.tool.as_str(), "create_plate");
    }
}
