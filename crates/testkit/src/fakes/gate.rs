use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::{ApprovalAnswer, GateDecision};
use contracts::{
    Error, ErrorCode, ExecutionContext, Gate, GateOutcome, GateRequest, GateSubjectKind,
};

use crate::builders::id;

enum Behavior {
    Fixed(GateDecision),
    /// `ask` until a human answers: approve gives `allow`, reject gives `deny`.
    Approval,
}

pub struct FakeGate {
    id: Identifier,
    kind: GateSubjectKind,
    behavior: Behavior,
}

impl FakeGate {
    /// Always returns `decision`.
    pub fn fixed(gate_id: &str, kind: GateSubjectKind, decision: GateDecision) -> Self {
        Self {
            id: id(gate_id),
            kind,
            behavior: Behavior::Fixed(decision),
        }
    }

    /// Asks for approval and follows the human's answer once there is one.
    pub fn approval(gate_id: &str, kind: GateSubjectKind) -> Self {
        Self {
            id: id(gate_id),
            kind,
            behavior: Behavior::Approval,
        }
    }
}

#[async_trait]
impl Gate for FakeGate {
    fn id(&self) -> &Identifier {
        &self.id
    }

    fn subject_kind(&self) -> GateSubjectKind {
        self.kind
    }

    async fn decide(
        &self,
        request: GateRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error> {
        ctx.ensure_not_cancelled()?;
        if request.subject.kind() != self.kind {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("gate {} only decides {:?}", self.id.as_str(), self.kind),
            ));
        }
        let (decision, reason) = match (&self.behavior, request.approval) {
            (Behavior::Fixed(decision), _) => (*decision, "fixed decision"),
            (Behavior::Approval, None) => (GateDecision::Ask, "human approval required"),
            (Behavior::Approval, Some(ApprovalAnswer::Approve)) => {
                (GateDecision::Allow, "approved by a human")
            }
            (Behavior::Approval, Some(ApprovalAnswer::Reject)) => {
                (GateDecision::Deny, "rejected by a human")
            }
        };
        Ok(GateOutcome {
            decision,
            reasons: vec![reason.to_owned()],
            evidence_ids: Vec::new(),
            rules: Vec::new(),
        })
    }
}
