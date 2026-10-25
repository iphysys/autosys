//! What a job's status says. The status is one tagged type (`JobStatus` in
//! `schemas/jobs/job.schema.json`): its tag is the state the stores index and
//! claim by, and it carries only what that state needs. A stop reason the
//! state implies is not stored; [`stop_reason`] supplies it.

use schema_types::{CompletedStopReason, JobState, JobStatus, StopReason, WakeCondition};

/// The state `status` is tagged with.
pub fn job_state(status: &JobStatus) -> JobState {
    match status {
        JobStatus::Queued { .. } => JobState::Queued,
        JobStatus::Running => JobState::Running,
        JobStatus::Waiting { .. } => JobState::Waiting,
        JobStatus::Suspended { .. } => JobState::Suspended,
        JobStatus::Completed { .. } => JobState::Completed,
        JobStatus::Cancelled { .. } => JobState::Cancelled,
        JobStatus::Failed { .. } => JobState::Failed,
    }
}

/// The stop reason `status` records or implies: a suspended job stopped with
/// `approval_required`, a cancelled one with `cancelled`. `None` while the
/// job is queued, running or waiting, and for a failed job, which records a
/// failure instead.
pub fn stop_reason(status: &JobStatus) -> Option<StopReason> {
    match status {
        JobStatus::Completed { stop_reason, .. } => Some(match stop_reason {
            CompletedStopReason::Success => StopReason::Success,
            CompletedStopReason::Failure => StopReason::Failure,
            CompletedStopReason::BudgetExhausted => StopReason::BudgetExhausted,
        }),
        JobStatus::Suspended { .. } => Some(StopReason::ApprovalRequired),
        JobStatus::Cancelled { .. } => Some(StopReason::Cancelled),
        JobStatus::Queued { .. }
        | JobStatus::Running
        | JobStatus::Waiting { .. }
        | JobStatus::Failed { .. } => None,
    }
}

/// What a waiting or suspended job waits for; `None` in any other state.
pub fn wake_condition(status: &JobStatus) -> Option<&WakeCondition> {
    match status {
        JobStatus::Waiting { wake_condition, .. } | JobStatus::Suspended { wake_condition, .. } => {
            Some(wake_condition)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn status(value: serde_json::Value) -> JobStatus {
        serde_json::from_value(value).expect("a valid status")
    }

    fn checkpoint() -> serde_json::Value {
        json!({ "version": 3, "sha256": format!("sha256:{}", "a".repeat(64)) })
    }

    fn wake() -> serde_json::Value {
        json!({ "kind": "approval_answered", "approval_id": "appr-1" })
    }

    #[test]
    fn the_tag_is_the_state() {
        let cases = [
            (json!({ "state": "queued" }), JobState::Queued),
            (
                json!({ "state": "queued", "checkpoint": checkpoint() }),
                JobState::Queued,
            ),
            (json!({ "state": "running" }), JobState::Running),
            (
                json!({ "state": "waiting", "wake_condition": wake(), "checkpoint": checkpoint() }),
                JobState::Waiting,
            ),
            (
                json!({ "state": "suspended", "wake_condition": wake(), "checkpoint": checkpoint() }),
                JobState::Suspended,
            ),
            (
                json!({ "state": "completed", "stop_reason": "success", "checkpoint": checkpoint() }),
                JobState::Completed,
            ),
            (
                json!({ "state": "cancelled", "checkpoint": checkpoint() }),
                JobState::Cancelled,
            ),
            (
                json!({
                    "state": "failed",
                    "failure": {
                        "error": { "code": "internal", "message": "broken", "retryable": false },
                        "kind": "harness_error"
                    },
                    "checkpoint": checkpoint()
                }),
                JobState::Failed,
            ),
        ];
        for (value, state) in cases {
            let parsed = status(value.clone());
            assert_eq!(job_state(&parsed), state, "{value}");
            assert_eq!(
                serde_json::to_value(&parsed).expect("serializes"),
                value,
                "the status round-trips"
            );
        }
    }

    #[test]
    fn implied_stop_reasons_come_from_the_tag() {
        let suspended = status(
            json!({ "state": "suspended", "wake_condition": wake(), "checkpoint": checkpoint() }),
        );
        let cancelled = status(json!({ "state": "cancelled", "checkpoint": checkpoint() }));
        let completed = status(json!({
            "state": "completed", "stop_reason": "budget_exhausted", "checkpoint": checkpoint()
        }));
        assert_eq!(stop_reason(&suspended), Some(StopReason::ApprovalRequired));
        assert_eq!(stop_reason(&cancelled), Some(StopReason::Cancelled));
        assert_eq!(stop_reason(&completed), Some(StopReason::BudgetExhausted));
        assert_eq!(stop_reason(&JobStatus::Running), None);
        assert_eq!(stop_reason(&JobStatus::Queued { checkpoint: None }), None);
    }

    #[test]
    fn only_a_parked_job_has_a_wake_condition() {
        let suspended = status(
            json!({ "state": "suspended", "wake_condition": wake(), "checkpoint": checkpoint() }),
        );
        assert!(wake_condition(&suspended).is_some());
        assert!(wake_condition(&JobStatus::Running).is_none());
        assert!(wake_condition(&JobStatus::Queued { checkpoint: None }).is_none());
    }
}
