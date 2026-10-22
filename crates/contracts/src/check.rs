//! Checks evaluate a subject and produce findings; the evaluator turns each
//! finding into an evidence record (blueprint section 14.6).

use async_trait::async_trait;
use schema_types::{CheckInput, CheckMethod, SdrDocument, SemVer, Transition, Verdict};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{CheckId, ElementId};

#[derive(Clone, Debug, PartialEq)]
pub struct CheckDescriptor {
    pub id: CheckId,
    pub version: SemVer,
    pub method: CheckMethod,
}

/// What a check evaluates.
#[derive(Clone, Copy, Debug)]
pub enum CheckSubject<'a> {
    /// A proposed transition against the document it is based on.
    Transition {
        base: &'a SdrDocument,
        transition: &'a Transition,
    },
    /// A whole SDR document version.
    Document(&'a SdrDocument),
}

/// One outcome of a check. The evaluator (PR 6) adds the evidence id, run,
/// timestamp and SDR version, and stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckFinding {
    pub subject_ids: Vec<ElementId>,
    pub inputs: Vec<CheckInput>,
    pub verdict: Verdict,
    pub message: Option<String>,
}

#[async_trait]
pub trait Check: Send + Sync {
    fn descriptor(&self) -> &CheckDescriptor;

    /// Evaluates without side effects. A deterministic check returns the same
    /// findings for the same subject. Returns `Cancelled` if `ctx` is cancelled.
    async fn evaluate(
        &self,
        subject: CheckSubject<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Vec<CheckFinding>, Error>;
}
