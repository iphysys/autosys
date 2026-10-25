use std::sync::Arc;

use contracts::ids::{EvidenceId, ProjectId, SdrVersion, TransitionId};
use contracts::types::{
    Actor, DesignDefinition, ElementKind, HarnessVersions, NonEmptyString, Operation, Requirement,
    SdrDocument, SdrDocumentSchemaVersion, SdrSlice, SdrVersionRecord,
    SdrVersionRecordSchemaVersion, Sha256, Timestamp, Transition,
};
use contracts::{Clock, Error, ErrorCode, ExecutionContext, SdrRepository};
use serde_json::Value;
use sha2::Digest;

use crate::apply::apply;
use crate::migrations;
use crate::schema::Schemas;
use crate::store::Store;
use crate::transitions::{
    InvalidTransition, Problem, ProblemKind, ProposedTransition, Rejection, Validated,
    ValidatedTransition, Validation,
};

/// What a commit records besides the transition itself.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitInfo {
    /// Evidence the commit gate relied on.
    pub evidence_ids: Vec<EvidenceId>,
    pub harness: HarnessVersions,
}

/// Version 0: the project's requirements, an empty design, and where they
/// came from.
#[derive(Clone, Debug, PartialEq)]
pub struct InitialVersion {
    pub transition_id: TransitionId,
    pub actor: Actor,
    pub reason: NonEmptyString,
    pub harness: HarnessVersions,
    /// The requirements a run starts from. They cite no evidence: nothing
    /// has been checked yet.
    pub requirements: Vec<Requirement>,
}

/// The only way to change design state (blueprint section 7.2). Versions are
/// linear (ADR-0020): a transition is validated against the head and commits
/// as head + 1.
pub struct SdrManager {
    store: Store,
    clock: Arc<dyn Clock>,
    schemas: Schemas,
}

impl SdrManager {
    /// `clock` stamps `committed_at` on version records.
    pub fn new(repository: Arc<dyn SdrRepository>, clock: Arc<dyn Clock>) -> Result<Self, Error> {
        Ok(Self {
            store: Store::new(repository),
            clock,
            schemas: Schemas::load()?,
        })
    }

    /// Stores version 0: the given requirements and an empty design, checked
    /// by the rules a transition's `add_requirement` meets, and by
    /// `system.schema.json`. InvalidInput listing every problem; Conflict if
    /// the project already has versions.
    pub async fn create_project(
        &self,
        project_id: &ProjectId,
        initial: InitialVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionRecord, Error> {
        let document = self.initial_document(project_id, &initial.requirements)?;
        if self.store.head(project_id, ctx).await?.is_some() {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!("project {} already has an SDR", project_id.as_str()),
            ));
        }
        let record = self.record(
            &document,
            initial.transition_id,
            initial.actor,
            initial.reason,
            CommitInfo {
                evidence_ids: Vec::new(),
                harness: initial.harness,
            },
        )?;
        self.store.append(&document, &record, ctx).await?;
        Ok(record)
    }

    /// The latest version, or `None` before the project exists.
    pub async fn head(
        &self,
        project_id: &ProjectId,
        ctx: &ExecutionContext,
    ) -> Result<Option<SdrVersion>, Error> {
        self.store.head(project_id, ctx).await
    }

    /// The record of `version`: who committed it, why, on what evidence, and
    /// with which harness. NotFound if it does not exist.
    pub async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionRecord, Error> {
        self.store.version_record(project_id, version, ctx).await
    }

    /// The document at `version`, keeping only the elements in `slice`: those
    /// of the listed kinds, plus the listed ids.
    pub async fn read(
        &self,
        project_id: &ProjectId,
        slice: &SdrSlice,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrDocument, Error> {
        let mut doc = self.store.document(project_id, version, ctx).await?;
        let keep = |kind: ElementKind, id: &str| {
            slice.kinds.contains(&kind) || slice.ids.iter().any(|i| i.as_str() == id)
        };
        doc.requirements
            .retain(|r| keep(ElementKind::Requirement, r.id.as_str()));
        doc.design
            .parameters
            .retain(|p| keep(ElementKind::Parameter, p.id.as_str()));
        doc.design
            .decisions
            .retain(|d| keep(ElementKind::Decision, d.id.as_str()));
        doc.design
            .assumptions
            .retain(|a| keep(ElementKind::Assumption, a.id.as_str()));
        Ok(doc)
    }

    /// Starts a transition on its way to validation.
    pub fn propose(&self, project_id: ProjectId, transition: Transition) -> ProposedTransition {
        ProposedTransition {
            project_id,
            transition,
        }
    }

    /// Checks the transition against `transition.schema.json`, including the
    /// constraints the Rust types do not enforce, then against the head:
    /// its base must be the head, its operations must apply cleanly, and the
    /// resulting document must satisfy `system.schema.json`.
    ///
    /// Problems with the transition come back as [`Validation::Invalid`];
    /// `Err` means validation itself could not run.
    pub async fn validate(
        &self,
        proposed: ProposedTransition,
        ctx: &ExecutionContext,
    ) -> Result<Validation, Error> {
        let ProposedTransition {
            project_id,
            transition,
        } = proposed;
        let mut problems: Vec<Problem> = self
            .schemas
            .transition_errors(&transition)?
            .into_iter()
            .map(|e| Problem::new(ProblemKind::Schema, e))
            .collect();

        let base_version = transition.base_version.clone();
        let mut next = None;
        match self.store.head(&project_id, ctx).await? {
            None => problems.push(Problem::new(
                ProblemKind::UnknownBase,
                format!("project {} has no SDR versions", project_id.as_str()),
            )),
            Some(head) if base_version.0 < head.0 => problems.push(Problem::new(
                ProblemKind::StaleBase,
                format!(
                    "base version {} is stale; the head is {}",
                    base_version.0, head.0
                ),
            )),
            Some(head) if base_version.0 > head.0 => problems.push(Problem::new(
                ProblemKind::UnknownBase,
                format!(
                    "base version {} does not exist; the head is {}",
                    base_version.0, head.0
                ),
            )),
            Some(head) => next = Some(next_version(&head)?),
        }

        let mut candidate = None;
        if let Some(next) = next {
            let base = self.store.document(&project_id, &base_version, ctx).await?;
            let (document, semantic) = apply(&base, next, &transition.operations);
            problems.extend(semantic);
            if problems.is_empty() {
                problems.extend(
                    self.schemas
                        .document_errors(&document)?
                        .into_iter()
                        .map(|e| Problem::new(ProblemKind::DocumentSchema, e)),
                );
            }
            candidate = Some((base, document));
        }

        match candidate {
            Some((base, document)) if problems.is_empty() => {
                Ok(Validation::Valid(ValidatedTransition {
                    inner: Box::new(Validated {
                        project_id,
                        transition,
                        base,
                        document,
                    }),
                }))
            }
            _ => Ok(Validation::Invalid(InvalidTransition {
                project_id,
                transition_id: transition.id,
                base_version,
                problems,
            })),
        }
    }

    /// Stores the validated document as the next version and returns its
    /// record. Conflict if another version was committed since validation:
    /// the transition must then be proposed again on the new head.
    pub async fn commit(
        &self,
        validated: ValidatedTransition,
        info: CommitInfo,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionRecord, Error> {
        let Validated {
            transition,
            document,
            ..
        } = *validated.inner;
        let record = self.record(
            &document,
            transition.id,
            transition.actor,
            transition.reason,
            info,
        )?;
        self.store.append(&document, &record, ctx).await?;
        Ok(record)
    }

    /// The record of the version that `transition_id` committed on `base`,
    /// if it did: after a crash, whether a commit landed before its
    /// `sdr_transition` was written (ADR-0031). Versions are linear
    /// (ADR-0020), so only `base + 1` can hold it; `None` if that version
    /// does not exist or another transition made it.
    pub async fn find_commit(
        &self,
        project_id: &ProjectId,
        transition_id: &TransitionId,
        base: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<Option<SdrVersionRecord>, Error> {
        let next = SdrVersion(base.0 + 1);
        match self.store.head(project_id, ctx).await? {
            Some(head) if head.0 >= next.0 => {}
            _ => return Ok(None),
        }
        let record = self.store.version_record(project_id, &next, ctx).await?;
        Ok((&record.transition_id == transition_id).then_some(record))
    }

    /// Refuses a validated transition, for example on a gate's decision.
    /// Consumes it, so it can never be committed.
    pub fn reject(&self, validated: ValidatedTransition, reasons: Vec<String>) -> Rejection {
        let Validated {
            project_id,
            transition,
            ..
        } = *validated.inner;
        Rejection {
            project_id,
            transition_id: transition.id,
            base_version: transition.base_version,
            reasons,
        }
    }

    /// Brings a raw stored or imported document up to the current schema
    /// version and checks it against `system.schema.json`.
    pub fn upgrade_document(&self, raw: Value) -> Result<SdrDocument, Error> {
        let current = migrations::upgrade(raw)?;
        let errors = self.schemas.raw_document_errors(&current)?;
        if !errors.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("the document breaks the SDR schema: {}", errors.join("; ")),
            ));
        }
        serde_json::from_value(current).map_err(|e| {
            Error::new(
                ErrorCode::InvalidInput,
                "the document does not match the SDR types",
            )
            .with_source(e)
        })
    }

    /// Version 0 of `project_id` holding `requirements`, or every problem
    /// with them.
    fn initial_document(
        &self,
        project_id: &ProjectId,
        requirements: &[Requirement],
    ) -> Result<SdrDocument, Error> {
        let empty = SdrDocument {
            schema_version: SdrDocumentSchemaVersion::X01,
            project_id: project_id.clone(),
            version: SdrVersion(0),
            parent_version: None,
            requirements: Vec::new(),
            design: DesignDefinition {
                parameters: Vec::new(),
                decisions: Vec::new(),
                assumptions: Vec::new(),
            },
        };
        let mut problems: Vec<String> = requirements
            .iter()
            .filter(|r| !r.evidence_ids.is_empty())
            .map(|r| {
                format!(
                    "requirement {} cites evidence; version 0 cites none",
                    r.id.as_str()
                )
            })
            .collect();
        let operations: Vec<Operation> = requirements
            .iter()
            .map(|requirement| Operation::AddRequirement {
                requirement: requirement.clone(),
            })
            .collect();
        let (mut document, semantic) = apply(&empty, SdrVersion(0), &operations);
        // Version 0 has no parent; apply() names the base as one.
        document.parent_version = None;
        problems.extend(semantic.into_iter().map(|p| p.message));
        if problems.is_empty() {
            problems.extend(self.schemas.document_errors(&document)?);
        }
        if problems.is_empty() {
            Ok(document)
        } else {
            Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "the requirements of project {} are invalid: {}",
                    project_id.as_str(),
                    problems.join("; ")
                ),
            ))
        }
    }

    fn record(
        &self,
        document: &SdrDocument,
        transition_id: TransitionId,
        actor: Actor,
        reason: NonEmptyString,
        info: CommitInfo,
    ) -> Result<SdrVersionRecord, Error> {
        Ok(SdrVersionRecord {
            schema_version: SdrVersionRecordSchemaVersion::X01,
            project_id: document.project_id.clone(),
            version: document.version.clone(),
            parent_version: document.parent_version.clone(),
            transition_id,
            actor,
            reason,
            committed_at: Timestamp(self.clock.now()),
            evidence_ids: info.evidence_ids,
            document_sha256: document_sha256(document)?,
            harness: info.harness,
        })
    }
}

fn next_version(head: &SdrVersion) -> Result<SdrVersion, Error> {
    head.0
        .checked_add(1)
        .map(SdrVersion)
        .ok_or_else(|| Error::new(ErrorCode::Internal, "the SDR version counter is exhausted"))
}

/// sha256 of the document's compact JSON, which serde writes in a fixed
/// field order.
pub(crate) fn document_sha256(document: &SdrDocument) -> Result<Sha256, Error> {
    let json = serde_json::to_vec(document).map_err(|e| {
        Error::new(ErrorCode::Internal, "cannot serialize the SDR document").with_source(e)
    })?;
    let hex: String = sha2::Sha256::digest(&json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "computed digest is not a valid Sha256"))
}
