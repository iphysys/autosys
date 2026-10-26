//! The gates of Increment 1. Each takes its policy, already loaded and
//! validated, at construction; none does I/O.

use std::collections::HashMap;

use async_trait::async_trait;
use contracts::ids::{CheckId, ElementId, EvidenceId, Identifier};
use contracts::types::{
    ApprovalAnswer, ApprovalRule, ApprovalTrigger, AutonomyPolicy, ElementKind, Evidence,
    EvidenceBody, GateDecision, GatesPolicy, Operation, SdrDocument, SdrSlice, ToolOverlay,
    ToolPolicy, Transition, Verdict,
};
use contracts::{
    Error, ErrorCode, ExecutionContext, Gate, GateOutcome, GateRequest, GateSubject,
    GateSubjectKind,
};

fn identifier(id: &str) -> Result<Identifier, Error> {
    id.parse()
        .map_err(|_| Error::new(ErrorCode::Internal, format!("invalid gate id {id}")))
}

fn outcome(
    decision: GateDecision,
    reasons: Vec<String>,
    evidence_ids: Vec<EvidenceId>,
) -> GateOutcome {
    GateOutcome {
        decision,
        reasons,
        evidence_ids,
        rules: Vec::new(),
    }
}

fn wrong_subject(gate: &str) -> Error {
    Error::new(
        ErrorCode::InvalidInput,
        format!("gate {gate} was asked about a subject it does not decide"),
    )
}

/// Gate `tool_permission`: a tool call goes ahead only if the tool is listed
/// in a `config/tools/` overlay. A tool the overlay marks `requires_approval`
/// waits for a human answer.
pub struct ToolPermissionGate {
    id: Identifier,
    tools: HashMap<String, ToolPolicy>,
}

impl ToolPermissionGate {
    pub const ID: &'static str = "tool_permission";

    /// InvalidInput if two overlays list the same tool name.
    pub fn new(overlays: &[ToolOverlay]) -> Result<Self, Error> {
        let mut tools = HashMap::new();
        for overlay in overlays {
            for policy in &overlay.tools {
                if tools
                    .insert(policy.name.as_str().to_owned(), policy.clone())
                    .is_some()
                {
                    return Err(Error::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "tool {} is listed twice in config/tools/",
                            policy.name.as_str()
                        ),
                    ));
                }
            }
        }
        Ok(Self {
            id: identifier(Self::ID)?,
            tools,
        })
    }
}

#[async_trait]
impl Gate for ToolPermissionGate {
    fn id(&self) -> &Identifier {
        &self.id
    }

    fn subject_kind(&self) -> GateSubjectKind {
        GateSubjectKind::ToolCall
    }

    async fn decide(
        &self,
        request: GateRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error> {
        ctx.ensure_not_cancelled()?;
        let GateSubject::ToolCall { request: call, .. } = request.subject else {
            return Err(wrong_subject(Self::ID));
        };
        let tool = call.tool.as_str();
        let Some(policy) = self.tools.get(tool) else {
            return Ok(outcome(
                GateDecision::Deny,
                vec![format!("tool {tool} is not listed in config/tools/")],
                Vec::new(),
            ));
        };
        Ok(match (policy.requires_approval, request.approval) {
            (false, _) => outcome(GateDecision::Allow, Vec::new(), Vec::new()),
            (true, None) => outcome(
                GateDecision::Ask,
                vec![format!("tool {tool} requires approval (config/tools/)")],
                Vec::new(),
            ),
            (true, Some(ApprovalAnswer::Approve)) => outcome(
                GateDecision::Allow,
                vec![format!("a human approved calling {tool}")],
                Vec::new(),
            ),
            (true, Some(ApprovalAnswer::Reject)) => outcome(
                GateDecision::Deny,
                vec![format!("a human refused calling {tool}")],
                Vec::new(),
            ),
        })
    }
}

/// Gate `sdr_commit`: a design acceptance gate (blueprint section 15.2).
/// Every check `config/policies/gates.yaml` requires must have evidence for
/// the transition:
///
/// - a required check without evidence: `deny` (fail closed);
/// - any `fail`: `reject_transition`, with that evidence attached;
/// - any `error`: `retry`;
/// - otherwise `allow`, with the evidence relied on attached. `unverified`
///   lets the commit through but never counts as passing.
pub struct SdrCommitGate {
    id: Identifier,
    required: Vec<CheckId>,
}

impl SdrCommitGate {
    pub const ID: &'static str = "sdr_commit";

    pub fn new(policy: &GatesPolicy) -> Result<Self, Error> {
        Ok(Self {
            id: identifier(Self::ID)?,
            required: policy.sdr_commit.required_checks.clone(),
        })
    }
}

#[async_trait]
impl Gate for SdrCommitGate {
    fn id(&self) -> &Identifier {
        &self.id
    }

    fn subject_kind(&self) -> GateSubjectKind {
        GateSubjectKind::SdrCommit
    }

    async fn decide(
        &self,
        request: GateRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error> {
        ctx.ensure_not_cancelled()?;
        let GateSubject::SdrCommit { evidence, .. } = request.subject else {
            return Err(wrong_subject(Self::ID));
        };
        let mut missing = Vec::new();
        let mut failed = (Vec::new(), Vec::new());
        let mut errored = (Vec::new(), Vec::new());
        let mut relied_on = Vec::new();
        for check in &self.required {
            let results: Vec<(&Evidence, &Verdict, Option<&str>)> = evidence
                .iter()
                .filter_map(|e| match &e.body {
                    EvidenceBody::Check {
                        check_id,
                        verdict,
                        message,
                        ..
                    } if check_id == check => {
                        Some((e, verdict, message.as_ref().map(|m| m.as_str())))
                    }
                    _ => None,
                })
                .collect();
            if results.is_empty() {
                missing.push(format!(
                    "required check {} produced no evidence",
                    check.as_str()
                ));
                continue;
            }
            for (record, verdict, message) in results {
                let reason = format!(
                    "{} {}: {}",
                    check.as_str(),
                    verdict_name(*verdict),
                    message.unwrap_or("no message")
                );
                match verdict {
                    Verdict::Fail => {
                        failed.0.push(reason);
                        failed.1.push(record.id.clone());
                    }
                    Verdict::Error => {
                        errored.0.push(reason);
                        errored.1.push(record.id.clone());
                    }
                    Verdict::Pass | Verdict::Unverified => relied_on.push(record.id.clone()),
                }
            }
        }
        Ok(if !missing.is_empty() {
            outcome(GateDecision::Deny, missing, Vec::new())
        } else if !failed.0.is_empty() {
            outcome(GateDecision::RejectTransition, failed.0, failed.1)
        } else if !errored.0.is_empty() {
            outcome(GateDecision::Retry, errored.0, errored.1)
        } else {
            outcome(GateDecision::Allow, Vec::new(), relied_on)
        })
    }
}

/// Gate `sdr_write_scope`: a transition may change only what its task's
/// write scope covers (blueprint sections 2.6 and 15.2). Every element an
/// operation adds, replaces or links evidence to must be of a kind the scope
/// lists, or have an id it lists; otherwise `deny`, naming each element.
///
/// The scope belongs to one task, so a gate is built per run. The read slice
/// needs no gate: `SdrManager::read` already returns only what it covers.
pub struct WriteScopeGate {
    id: Identifier,
    scope: SdrSlice,
}

impl WriteScopeGate {
    pub const ID: &'static str = "sdr_write_scope";

    pub fn new(scope: &SdrSlice) -> Result<Self, Error> {
        Ok(Self {
            id: identifier(Self::ID)?,
            scope: scope.clone(),
        })
    }

    fn covers(&self, kind: Option<ElementKind>, id: &ElementId) -> bool {
        kind.is_some_and(|k| self.scope.kinds.contains(&k)) || self.scope.ids.contains(id)
    }

    fn describe(&self) -> String {
        let kinds: Vec<String> = self.scope.kinds.iter().map(ToString::to_string).collect();
        let ids: Vec<String> = self
            .scope
            .ids
            .iter()
            .map(|i| i.as_str().to_owned())
            .collect();
        format!("kinds: {}; ids: {}", or_none(&kinds), or_none(&ids))
    }
}

fn or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

/// The kind of the element an operation touches, and its id. For
/// `link_evidence` the kind comes from the base document or from an element
/// the same transition adds; `None` if neither has it.
fn touched<'a>(
    operation: &'a Operation,
    base: &SdrDocument,
    transition: &Transition,
) -> (Option<ElementKind>, &'a ElementId) {
    match operation {
        Operation::AddRequirement { requirement } => {
            (Some(ElementKind::Requirement), &requirement.id)
        }
        Operation::SetParameter { parameter } => (Some(ElementKind::Parameter), &parameter.id),
        Operation::AddDecision { decision } => (Some(ElementKind::Decision), &decision.id),
        Operation::AddAssumption { assumption } => (Some(ElementKind::Assumption), &assumption.id),
        Operation::LinkEvidence { element_id, .. } => {
            let added = transition.operations.iter().find_map(|op| match op {
                Operation::AddRequirement { requirement } if &requirement.id == element_id => {
                    Some(ElementKind::Requirement)
                }
                Operation::SetParameter { parameter } if &parameter.id == element_id => {
                    Some(ElementKind::Parameter)
                }
                _ => None,
            });
            (added.or_else(|| kind_in(base, element_id)), element_id)
        }
    }
}

fn kind_in(doc: &SdrDocument, id: &ElementId) -> Option<ElementKind> {
    if doc.requirements.iter().any(|r| &r.id == id) {
        Some(ElementKind::Requirement)
    } else if doc.design.parameters.iter().any(|p| &p.id == id) {
        Some(ElementKind::Parameter)
    } else if doc.design.decisions.iter().any(|d| &d.id == id) {
        Some(ElementKind::Decision)
    } else if doc.design.assumptions.iter().any(|a| &a.id == id) {
        Some(ElementKind::Assumption)
    } else {
        None
    }
}

#[async_trait]
impl Gate for WriteScopeGate {
    fn id(&self) -> &Identifier {
        &self.id
    }

    fn subject_kind(&self) -> GateSubjectKind {
        GateSubjectKind::SdrCommit
    }

    async fn decide(
        &self,
        request: GateRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<GateOutcome, Error> {
        ctx.ensure_not_cancelled()?;
        let GateSubject::SdrCommit {
            base, transition, ..
        } = request.subject
        else {
            return Err(wrong_subject(Self::ID));
        };
        let outside: Vec<String> = transition
            .operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| {
                let (kind, id) = touched(operation, base, transition);
                (!self.covers(kind, id)).then(|| {
                    let what = kind.map_or_else(|| "element".to_owned(), |k| k.to_string());
                    format!(
                        "operation {index}: {what} {} is outside the task's write scope ({})",
                        id.as_str(),
                        self.describe()
                    )
                })
            })
            .collect();
        Ok(if outside.is_empty() {
            outcome(GateDecision::Allow, Vec::new(), Vec::new())
        } else {
            outcome(GateDecision::Deny, outside, Vec::new())
        })
    }
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "passed",
        Verdict::Fail => "failed",
        Verdict::Unverified => "unverified",
        Verdict::Error => "errored",
    }
}

/// Gate `approval.sdr_commit` or `approval.tool_call`: asks a human when a
/// rule in `config/policies/autonomy.yaml` matches. An action no rule matches
/// needs no approval.
pub struct ApprovalGate {
    id: Identifier,
    kind: GateSubjectKind,
    rules: Vec<ApprovalRule>,
}

impl ApprovalGate {
    pub const SDR_COMMIT_ID: &'static str = "approval.sdr_commit";
    pub const TOOL_CALL_ID: &'static str = "approval.tool_call";

    /// The gate for one kind of subject, with the policy's rules for it.
    pub fn new(policy: &AutonomyPolicy, kind: GateSubjectKind) -> Result<Self, Error> {
        let id = match kind {
            GateSubjectKind::SdrCommit => Self::SDR_COMMIT_ID,
            GateSubjectKind::ToolCall => Self::TOOL_CALL_ID,
        };
        let rules = policy
            .approvals
            .iter()
            .filter(|rule| {
                matches!(
                    (&rule.trigger, kind),
                    (
                        ApprovalTrigger::SdrCommit { .. },
                        GateSubjectKind::SdrCommit
                    ) | (ApprovalTrigger::ToolCall { .. }, GateSubjectKind::ToolCall)
                )
            })
            .cloned()
            .collect();
        Ok(Self {
            id: identifier(id)?,
            kind,
            rules,
        })
    }

    fn matching(&self, subject: &GateSubject<'_>) -> Vec<&ApprovalRule> {
        self.rules
            .iter()
            .filter(|rule| match (&rule.trigger, subject) {
                (
                    ApprovalTrigger::SdrCommit { final_only },
                    GateSubject::SdrCommit { transition, .. },
                ) => !final_only || transition.final_,
                (ApprovalTrigger::ToolCall { tool }, GateSubject::ToolCall { request, .. }) => {
                    &request.tool == tool
                }
                _ => false,
            })
            .collect()
    }
}

#[async_trait]
impl Gate for ApprovalGate {
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
            return Err(wrong_subject(self.id.as_str()));
        }
        let rules = self.matching(&request.subject);
        if rules.is_empty() {
            return Ok(outcome(GateDecision::Allow, Vec::new(), Vec::new()));
        }
        let refused = match self.kind {
            GateSubjectKind::SdrCommit => GateDecision::RejectTransition,
            GateSubjectKind::ToolCall => GateDecision::Deny,
        };
        let names: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        let matched: Vec<Identifier> = rules.iter().map(|r| r.id.clone()).collect();
        let decided = match request.approval {
            None => outcome(
                GateDecision::Ask,
                rules
                    .iter()
                    .map(|r| format!("{}: {}", r.id.as_str(), r.description.as_str()))
                    .collect(),
                Vec::new(),
            ),
            Some(ApprovalAnswer::Approve) => outcome(
                GateDecision::Allow,
                vec![format!("a human approved ({})", names.join(", "))],
                Vec::new(),
            ),
            Some(ApprovalAnswer::Reject) => outcome(
                refused,
                vec![format!("a human refused ({})", names.join(", "))],
                Vec::new(),
            ),
        };
        Ok(GateOutcome {
            rules: matched,
            ..decided
        })
    }
}
