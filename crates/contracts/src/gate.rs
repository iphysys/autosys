//! Gates decide whether a proposed action or transition goes ahead
//! (blueprint section 15).

use async_trait::async_trait;
use schema_types::{ApprovalAnswer, Evidence, GateDecision, SdrDocument, Transition};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{AgentId, EvidenceId, Identifier};
use crate::tool::ToolRequest;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateSubjectKind {
    ToolCall,
    SdrCommit,
}

#[derive(Clone, Copy, Debug)]
pub enum GateSubject<'a> {
    ToolCall {
        agent_id: &'a AgentId,
        request: &'a ToolRequest,
    },
    SdrCommit {
        base: &'a SdrDocument,
        transition: &'a Transition,
        evidence: &'a [Evidence],
    },
}

impl GateSubject<'_> {
    pub fn kind(&self) -> GateSubjectKind {
        match self {
            GateSubject::ToolCall { .. } => GateSubjectKind::ToolCall,
            GateSubject::SdrCommit { .. } => GateSubjectKind::SdrCommit,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GateRequest<'a> {
    pub subject: GateSubject<'a>,
    /// A human answer already given for this exact subject, when the run
    /// resumes after an approval.
    pub approval: Option<ApprovalAnswer>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateOutcome {
    pub decision: GateDecision,
    pub reasons: Vec<String>,
    pub evidence_ids: Vec<EvidenceId>,
    /// The policy rules that matched, for an approval gate: what a human is
    /// asked about, recorded with the request (ADR-0035). Empty for other
    /// gates.
    pub rules: Vec<Identifier>,
}

#[async_trait]
pub trait Gate: Send + Sync {
    fn id(&self) -> &Identifier;

    /// The only kind of subject this gate is asked about.
    fn subject_kind(&self) -> GateSubjectKind;

    /// Callers treat `Err` as `deny`: gates fail closed (blueprint section
    /// 2.8). Returns `Cancelled` if `ctx` is cancelled.
    async fn decide(
        &self,
        request: GateRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error>;
}
