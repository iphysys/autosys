//! What the event log knows about a payload: its kind, which the log indexes
//! so a reader can ask for some kinds only, and the key of an event that
//! completes something, which the log stores once per run.

use std::fmt;

use schema_types::EventPayload;

/// The `type` tag of an event payload, as the event schema spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventKind(&'static str);

impl EventKind {
    pub const RUN_STARTED: EventKind = EventKind("run_started");
    pub const RUN_STOPPED: EventKind = EventKind("run_stopped");
    pub const JOB_STATE_CHANGED: EventKind = EventKind("job_state_changed");
    pub const CONTEXT_ASSEMBLED: EventKind = EventKind("context_assembled");
    pub const MODEL_CALL_COMPLETED: EventKind = EventKind("model_call_completed");
    pub const TOOL_CALL_STARTED: EventKind = EventKind("tool_call_started");
    pub const TOOL_CALL_COMPLETED: EventKind = EventKind("tool_call_completed");
    pub const CHECK_COMPLETED: EventKind = EventKind("check_completed");
    pub const GATE_DECIDED: EventKind = EventKind("gate_decided");
    pub const APPROVAL_REQUESTED: EventKind = EventKind("approval_requested");
    pub const APPROVAL_ANSWERED: EventKind = EventKind("approval_answered");
    pub const SDR_TRANSITION: EventKind = EventKind("sdr_transition");
    pub const ARTIFACT_PUBLISHED: EventKind = EventKind("artifact_published");
    pub const CANCEL_REQUESTED: EventKind = EventKind("cancel_requested");
    pub const ERROR: EventKind = EventKind("error");

    /// The kind of `payload`.
    pub fn of(payload: &EventPayload) -> Self {
        match payload {
            EventPayload::RunStarted { .. } => Self::RUN_STARTED,
            EventPayload::RunStopped { .. } => Self::RUN_STOPPED,
            EventPayload::JobStateChanged { .. } => Self::JOB_STATE_CHANGED,
            EventPayload::ContextAssembled { .. } => Self::CONTEXT_ASSEMBLED,
            EventPayload::ModelCallCompleted { .. } => Self::MODEL_CALL_COMPLETED,
            EventPayload::ToolCallStarted { .. } => Self::TOOL_CALL_STARTED,
            EventPayload::ToolCallCompleted { .. } => Self::TOOL_CALL_COMPLETED,
            EventPayload::CheckCompleted { .. } => Self::CHECK_COMPLETED,
            EventPayload::GateDecided { .. } => Self::GATE_DECIDED,
            EventPayload::ApprovalRequested { .. } => Self::APPROVAL_REQUESTED,
            EventPayload::ApprovalAnswered { .. } => Self::APPROVAL_ANSWERED,
            EventPayload::SdrTransition { .. } => Self::SDR_TRANSITION,
            EventPayload::ArtifactPublished { .. } => Self::ARTIFACT_PUBLISHED,
            EventPayload::CancelRequested { .. } => Self::CANCEL_REQUESTED,
            EventPayload::Error { .. } => Self::ERROR,
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// The key under which the log stores `payload` at most once per run, if it
/// completes something: `run_started` for its run, `tool_call_completed` for
/// its step, `sdr_transition` for its transition and `approval_requested` for
/// its approval. A worker that runs again after a crash may write such an
/// event a second time; the log keeps the first (ADR-0031).
pub fn completion_key(payload: &EventPayload) -> Option<String> {
    match payload {
        EventPayload::RunStarted { .. } => Some(EventKind::RUN_STARTED.as_str().to_owned()),
        EventPayload::ToolCallCompleted { step_id, .. } => Some(format!(
            "{}:{}",
            EventKind::TOOL_CALL_COMPLETED,
            step_id.as_str()
        )),
        EventPayload::SdrTransition { transition_id, .. } => Some(format!(
            "{}:{}",
            EventKind::SDR_TRANSITION,
            transition_id.as_str()
        )),
        EventPayload::ApprovalRequested { approval_id, .. } => Some(format!(
            "{}:{}",
            EventKind::APPROVAL_REQUESTED,
            approval_id.as_str()
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn payload(value: serde_json::Value) -> EventPayload {
        serde_json::from_value(value).expect("valid payload")
    }

    #[test]
    fn completion_events_have_keys_and_others_do_not() {
        let started = payload(json!({ "type": "run_started", "task_id": "t", "agent_id": "a" }));
        let completed = payload(json!({
            "type": "tool_call_completed", "step_id": "step-2.1", "tool": "measure",
            "outcome": "ok", "evidence_ids": [], "duration_ms": 5
        }));
        let transition = payload(json!({
            "type": "sdr_transition", "transition_id": "tr-1", "outcome": "rejected",
            "base_version": 1, "reasons": []
        }));
        let asked = payload(json!({
            "type": "approval_requested", "approval_id": "appr-1", "gate": "approval",
            "subject_id": "tr-1", "summary": "Commit the final design."
        }));
        let cancel = payload(json!({ "type": "cancel_requested" }));
        assert_eq!(completion_key(&started).as_deref(), Some("run_started"));
        assert_eq!(
            completion_key(&completed).as_deref(),
            Some("tool_call_completed:step-2.1")
        );
        assert_eq!(
            completion_key(&transition).as_deref(),
            Some("sdr_transition:tr-1")
        );
        assert_eq!(
            completion_key(&asked).as_deref(),
            Some("approval_requested:appr-1")
        );
        assert_eq!(completion_key(&cancel), None);
    }

    #[test]
    fn a_recovered_completion_has_the_same_key() {
        let first = payload(json!({
            "type": "tool_call_completed", "step_id": "step-2.1", "tool": "measure",
            "outcome": "ok", "evidence_ids": [], "duration_ms": 5
        }));
        let recovered = payload(json!({
            "type": "tool_call_completed", "step_id": "step-2.1", "tool": "measure",
            "outcome": "ok", "evidence_ids": [], "duration_ms": 0, "recovered": true
        }));
        assert_eq!(completion_key(&first), completion_key(&recovered));
    }
}
