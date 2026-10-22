//! The typestate of a transition: [`ProposedTransition`], then
//! [`ValidatedTransition`], then a committed version. Neither type can be
//! built outside this crate, and only `SdrManager::validate` creates a
//! `ValidatedTransition`, so `SdrManager::commit` never sees an unvalidated
//! transition.

use std::fmt;

use contracts::ids::{ProjectId, SdrVersion, TransitionId};
use contracts::types::{SdrDocument, Transition};

/// A transition submitted for validation.
#[derive(Debug)]
pub struct ProposedTransition {
    pub(crate) project_id: ProjectId,
    pub(crate) transition: Transition,
}

impl ProposedTransition {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn transition(&self) -> &Transition {
        &self.transition
    }
}

/// A transition that passed schema and semantic validation against the
/// current head. Checks and gates inspect it; only `commit` or `reject`
/// consume it.
#[derive(Debug)]
pub struct ValidatedTransition {
    pub(crate) inner: Box<Validated>,
}

/// The contents of a [`ValidatedTransition`], boxed so a `Validation` stays
/// small whichever way it went.
#[derive(Debug)]
pub(crate) struct Validated {
    pub(crate) project_id: ProjectId,
    pub(crate) transition: Transition,
    pub(crate) base: SdrDocument,
    pub(crate) document: SdrDocument,
}

impl ValidatedTransition {
    pub fn project_id(&self) -> &ProjectId {
        &self.inner.project_id
    }

    pub fn transition(&self) -> &Transition {
        &self.inner.transition
    }

    /// The version the transition was validated against.
    pub fn base(&self) -> &SdrDocument {
        &self.inner.base
    }

    /// The version a commit would store.
    pub fn document(&self) -> &SdrDocument {
        &self.inner.document
    }
}

/// The result of validating a proposed transition.
#[derive(Debug)]
pub enum Validation {
    Valid(ValidatedTransition),
    Invalid(InvalidTransition),
}

/// A proposed transition that failed validation, with every problem found.
/// The transition is consumed: it can be fixed and proposed again, never
/// committed.
#[derive(Clone, Debug, PartialEq)]
pub struct InvalidTransition {
    pub project_id: ProjectId,
    pub transition_id: TransitionId,
    pub base_version: SdrVersion,
    pub problems: Vec<Problem>,
}

impl InvalidTransition {
    pub fn has(&self, kind: ProblemKind) -> bool {
        self.problems.iter().any(|p| p.kind == kind)
    }
}

/// One reason a transition is invalid. The message is meant for whoever
/// repairs the transition, often the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub kind: ProblemKind,
    pub message: String,
}

impl Problem {
    pub(crate) fn new(kind: ProblemKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// The transition breaks `transition.schema.json`.
    Schema,
    /// The base version is older than the head. v0 never rebases.
    StaleBase,
    /// The base version does not exist yet, or the project has no versions.
    UnknownBase,
    /// An element id is already used in the document or the transition.
    DuplicateElement,
    /// An operation or reference names an element that does not exist.
    UnknownElement,
    /// Evidence linked to an element that holds none (decisions, assumptions).
    CannotHoldEvidence,
    /// The resulting document breaks `system.schema.json`.
    DocumentSchema,
}

/// A validated transition that a gate refused. Returned by
/// `SdrManager::reject`, which consumes the transition.
#[derive(Clone, Debug, PartialEq)]
pub struct Rejection {
    pub project_id: ProjectId,
    pub transition_id: TransitionId,
    pub base_version: SdrVersion,
    pub reasons: Vec<String>,
}
