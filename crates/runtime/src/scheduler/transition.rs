//! Job state changes as the log records them (ADR-0031): the one builder of
//! `job_state_changed`, and what a claim adds when records are missing.

use contracts::types::{
    Actor, Event, EventPayload, JobState, JobStatus, NonEmptyString, Timestamp,
};
use contracts::{
    Clock, Error, ErrorCode, EventKind, EventLog, NewEvent, WriteAuthority, job_state,
};

use contracts::ids::{CorrelationId, RunId};

/// The component recorded as the actor of the scheduler's events.
const COMPONENT: &str = "scheduler";

/// Why a job changed state, as the event's `reason` says it. A requeue
/// after a failed attempt says [`RETRY`], so a new owner that finishes it
/// knows to wait (ADR-0031).
pub(crate) const SUBMITTED: &str = "submitted";
pub(crate) const CLAIMED: &str = "claimed";
pub(crate) const RECLAIMED: &str = "reclaimed after its lease expired";
pub(crate) const WOKEN_BY_ANSWER: &str = "woken by an approval answer";
pub(crate) const WOKEN_BY_CANCEL: &str = "woken by a cancel request";
pub(crate) const RETRY: &str = "retry after a failed attempt";
pub(crate) const RELEASED: &str = "released by a worker shutting down";

/// `job_state_changed` from `from` (absent for a submit) into `to`.
pub(crate) fn changed(from: Option<JobState>, to: JobStatus, reason: &str) -> EventPayload {
    EventPayload::JobStateChanged {
        from,
        to,
        reason: reason.parse::<NonEmptyString>().ok(),
    }
}

/// Appends `payload` to `run` with `authority`.
pub(crate) async fn record(
    events: &dyn EventLog,
    clock: &dyn Clock,
    run: &RunId,
    correlation_id: &CorrelationId,
    payload: EventPayload,
    authority: &WriteAuthority,
) -> Result<Event, Error> {
    let component = COMPONENT
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "the scheduler's id is invalid"))?;
    events
        .append(
            NewEvent {
                run_id: run.clone(),
                occurred_at: Timestamp(clock.now()),
                correlation_id: correlation_id.clone(),
                actor: Actor::System { component },
                payload,
            },
            Some(authority),
        )
        .await
}

/// The run's last `job_state_changed`, with its seq.
pub(crate) async fn last_change(
    events: &dyn EventLog,
    run: &RunId,
) -> Result<Option<(u64, Option<JobState>, JobStatus)>, Error> {
    let mut last = None;
    let mut after = 0;
    loop {
        let page = events
            .read_kinds(run, after, &[EventKind::JOB_STATE_CHANGED], 256)
            .await?;
        let Some(tail) = page.last() else {
            return Ok(last);
        };
        after = tail.seq.get();
        if let EventPayload::JobStateChanged { from, to, .. } = &tail.payload {
            last = Some((after, *from, to.clone()));
        }
    }
}

/// The records a claim appends, under its fence, so that the chain of
/// `job_state_changed` has no gap and ends in running (ADR-0031). `last` is
/// the state the log ends in (`None` before any record) and its status;
/// `interrupted` whether the job was claimed from running.
///
/// A submit or a wake-up records itself after it acts, and a crash between
/// the two loses the record; so does a claim. Whatever is missing on the
/// way from the log's last state to this claim is written here.
pub(crate) fn missing(last: Option<&JobStatus>, interrupted: bool) -> Vec<EventPayload> {
    let mut records = Vec::new();
    let mut at = last.map(job_state);
    if at.is_none() {
        records.push(changed(
            None,
            JobStatus::Queued { checkpoint: None },
            SUBMITTED,
        ));
        at = Some(JobState::Queued);
    }
    if let Some(parked @ (JobState::Waiting | JobState::Suspended)) = at {
        let checkpoint = last.and_then(|status| match status {
            JobStatus::Waiting { checkpoint, .. } | JobStatus::Suspended { checkpoint, .. } => {
                Some(checkpoint.clone())
            }
            _ => None,
        });
        records.push(changed(
            Some(parked),
            JobStatus::Queued { checkpoint },
            WOKEN_BY_ANSWER,
        ));
        at = Some(JobState::Queued);
    }
    match at {
        Some(JobState::Queued) => {
            records.push(changed(Some(JobState::Queued), JobStatus::Running, CLAIMED));
            if interrupted {
                records.push(changed(
                    Some(JobState::Running),
                    JobStatus::Running,
                    RECLAIMED,
                ));
            }
        }
        Some(from) => {
            let reason = if interrupted { RECLAIMED } else { CLAIMED };
            records.push(changed(Some(from), JobStatus::Running, reason));
        }
        None => {}
    }
    records
}

#[cfg(test)]
mod tests {
    use contracts::types::CheckpointRef;
    use serde_json::json;

    use super::*;

    fn checkpoint() -> CheckpointRef {
        serde_json::from_value(
            json!({ "version": 3, "sha256": format!("sha256:{}", "c".repeat(64)) }),
        )
        .expect("a reference")
    }

    fn chain(records: &[EventPayload]) -> Vec<(Option<JobState>, JobState)> {
        records
            .iter()
            .map(|r| match r {
                EventPayload::JobStateChanged { from, to, .. } => (*from, job_state(to)),
                other => panic!("not a state change: {other:?}"),
            })
            .collect()
    }

    use JobState::{Queued, Running, Suspended};

    #[test]
    fn a_claim_records_itself_and_whatever_is_missing() {
        let queued = JobStatus::Queued { checkpoint: None };
        assert_eq!(
            chain(&missing(Some(&queued), false)),
            [(Some(Queued), Running)]
        );
        assert_eq!(
            chain(&missing(None, false)),
            [(None, Queued), (Some(Queued), Running)],
            "the submit's record was lost"
        );
        let suspended = JobStatus::Suspended {
            wake_condition: serde_json::from_value(
                json!({ "kind": "approval_answered", "approval_id": "appr-1" }),
            )
            .expect("a condition"),
            checkpoint: checkpoint(),
        };
        let woken = missing(Some(&suspended), false);
        assert_eq!(
            chain(&woken),
            [(Some(Suspended), Queued), (Some(Queued), Running)],
            "the wake-up's record was lost"
        );
        match &woken[0] {
            EventPayload::JobStateChanged {
                to: JobStatus::Queued {
                    checkpoint: Some(c),
                },
                ..
            } => assert_eq!(c, &checkpoint()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reclaim_records_itself_after_a_lost_claim_record() {
        assert_eq!(
            chain(&missing(Some(&JobStatus::Running), true)),
            [(Some(Running), Running)]
        );
        let queued = JobStatus::Queued { checkpoint: None };
        assert_eq!(
            chain(&missing(Some(&queued), true)),
            [(Some(Queued), Running), (Some(Running), Running)],
            "the earlier claim's record was lost"
        );
    }
}
