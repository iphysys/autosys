//! Lease fencing (ADR-0032): what authorises a store write, and the one rule
//! every store applies before it writes.
//!
//! A worker's authority is the [`FenceToken`] from its claim. A store checks
//! the token in the write's own transaction: the job must still be running
//! under that holder and epoch, with an unexpired lease. A few writes come
//! from outside any job; each names its [`UnfencedReason`], and
//! [`authorize`] permits it only the writes that reason covers. Everything
//! else fails closed with [`ErrorCode::LeaseLost`].
//!
//! None of these types is serializable: a token is never persisted or sent.

use std::fmt;

use schema_types::{EventPayload, JobState, JobStatus};

use crate::error::{Error, ErrorCode};
use crate::ids::{Identifier, JobId, SdrVersion};

/// Proof that a claim leased `job_id` to `holder` at `epoch`. Every claim
/// raises the job's epoch by one, so a token from an earlier claim is stale
/// whoever holds the job now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceToken {
    job_id: JobId,
    holder: Identifier,
    epoch: u64,
}

impl FenceToken {
    /// For `JobStore::claim` implementations only (arch-check).
    pub fn issue_for_claim(job_id: JobId, holder: Identifier, epoch: u64) -> Self {
        Self {
            job_id,
            holder,
            epoch,
        }
    }

    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    pub fn holder(&self) -> &Identifier {
        &self.holder
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl fmt::Display for FenceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{} on {}",
            self.holder.as_str(),
            self.epoch,
            self.job_id.as_str()
        )
    }
}

/// What authorises a store write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteAuthority {
    /// A job's writes, while its claim is current.
    Fenced(FenceToken),
    /// A write from outside any job, permitted only what its reason covers.
    Unfenced(UnfencedReason),
}

/// The writes that need no fence. Each reason names one operation and covers
/// what that operation writes, across stores: a job write and the event
/// that records it. This is the whole allow-list: a new reason is a new
/// variant, with a test, recorded in an ADR. Job-scoped writers never get
/// one.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnfencedReason {
    /// Submitting a top-level job: `JobStore::insert`, and the
    /// `job_state_changed` that records it, into queued without a `from` or a
    /// checkpoint. A child job is a fenced insert under its parent's fence,
    /// so a parent whose lease was reclaimed cannot spawn one.
    JobSubmit,
    /// Waking a waiting or suspended job: `JobStore::wake`, and the
    /// `job_state_changed` that records it, from waiting or suspended into
    /// queued on its checkpoint.
    JobWake,
    /// A command from a client: appending `approval_answered` or
    /// `cancel_requested`.
    ApiCommand,
    /// Creating a project: appending SDR version 0.
    ProjectCreate,
}

impl UnfencedReason {
    pub const ALL: [UnfencedReason; 4] = [
        UnfencedReason::JobSubmit,
        UnfencedReason::JobWake,
        UnfencedReason::ApiCommand,
        UnfencedReason::ProjectCreate,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            UnfencedReason::JobSubmit => "job_submit",
            UnfencedReason::JobWake => "job_wake",
            UnfencedReason::ApiCommand => "api_command",
            UnfencedReason::ProjectCreate => "project_create",
        }
    }

    /// Whether this reason permits `write`.
    pub fn permits(self, write: StoreWrite<'_>) -> bool {
        match (self, write) {
            (UnfencedReason::JobSubmit, StoreWrite::JobInsert)
            | (UnfencedReason::JobWake, StoreWrite::JobWake) => true,
            (
                UnfencedReason::JobSubmit,
                StoreWrite::Event(EventPayload::JobStateChanged {
                    from: None,
                    to: JobStatus::Queued { checkpoint: None },
                    ..
                }),
            ) => true,
            (
                UnfencedReason::JobWake,
                StoreWrite::Event(EventPayload::JobStateChanged {
                    from: Some(JobState::Waiting | JobState::Suspended),
                    to:
                        JobStatus::Queued {
                            checkpoint: Some(_),
                        },
                    ..
                }),
            ) => true,
            (UnfencedReason::ApiCommand, StoreWrite::Event(payload)) => matches!(
                payload,
                EventPayload::ApprovalAnswered { .. } | EventPayload::CancelRequested { .. }
            ),
            (UnfencedReason::ProjectCreate, StoreWrite::SdrVersion(version)) => version.0 == 0,
            _ => false,
        }
    }
}

impl fmt::Display for UnfencedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A store write, as [`authorize`] sees it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum StoreWrite<'a> {
    JobInsert,
    JobWake,
    Event(&'a EventPayload),
    StepBegin,
    StepComplete,
    SdrVersion(&'a SdrVersion),
}

impl fmt::Display for StoreWrite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreWrite::JobInsert => f.write_str("a job insert"),
            StoreWrite::JobWake => f.write_str("a job wake-up"),
            StoreWrite::Event(payload) => write!(f, "an event of type {}", payload_type(payload)),
            StoreWrite::StepBegin => f.write_str("a step begin"),
            StoreWrite::StepComplete => f.write_str("a step completion"),
            StoreWrite::SdrVersion(version) => write!(f, "SDR version {}", version.0),
        }
    }
}

/// The first thing every store does for a write.
///
/// - `Ok(Some(fence))`: verify `fence` in the write's transaction, before
///   any other condition.
/// - `Ok(None)`: an unfenced reason permits the write.
/// - `Err(LeaseLost)`: no authority, or a reason that does not permit it.
pub fn authorize<'a>(
    authority: Option<&'a WriteAuthority>,
    write: StoreWrite<'_>,
) -> Result<Option<&'a FenceToken>, Error> {
    match authority {
        Some(WriteAuthority::Fenced(fence)) => Ok(Some(fence)),
        Some(WriteAuthority::Unfenced(reason)) if reason.permits(write) => Ok(None),
        Some(WriteAuthority::Unfenced(reason)) => Err(Error::new(
            ErrorCode::LeaseLost,
            format!("unfenced reason {reason} does not permit {write}"),
        )),
        None => Err(Error::new(
            ErrorCode::LeaseLost,
            format!("{write} needs a write authority, and none was given"),
        )),
    }
}

/// Why a store refused a fence.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staleness {
    /// No job has the fence's id.
    MissingJob,
    /// A later claim raised the job's epoch.
    Reclaimed,
    /// The job is no longer running.
    NotRunning,
    /// The lease expired, and nobody has claimed the job since.
    Expired,
}

/// The job as a store found it after refusing a fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundLease {
    pub epoch: u64,
    pub holder: Option<Identifier>,
    pub running: bool,
}

/// A fence the store refused, with what it found instead. The source of
/// every [`ErrorCode::LeaseLost`] that a stale fence causes, so a caller can
/// log or count it without parsing the message.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("fence {fence} refused: {}", describe(self))]
pub struct StaleFence {
    pub fence: FenceToken,
    pub staleness: Staleness,
    /// The job's epoch now, unless it does not exist.
    pub current_epoch: Option<u64>,
    /// Who holds the job now, if anyone.
    pub current_holder: Option<Identifier>,
}

impl StaleFence {
    /// Classifies a refused `fence` from the job as found, `None` if it does
    /// not exist. The epoch decides first: a later claim makes a fence stale
    /// whatever else is true.
    pub fn new(fence: &FenceToken, found: Option<FoundLease>) -> Self {
        let Some(found) = found else {
            return Self {
                fence: fence.clone(),
                staleness: Staleness::MissingJob,
                current_epoch: None,
                current_holder: None,
            };
        };
        let staleness = if found.epoch != fence.epoch {
            Staleness::Reclaimed
        } else if !found.running {
            Staleness::NotRunning
        } else if found.holder.as_ref() != Some(&fence.holder) {
            // One epoch has one holder; a different one means another claim.
            Staleness::Reclaimed
        } else {
            Staleness::Expired
        };
        Self {
            fence: fence.clone(),
            staleness,
            current_epoch: Some(found.epoch),
            current_holder: found.holder,
        }
    }
}

fn describe(stale: &StaleFence) -> String {
    let job = stale.fence.job_id.as_str();
    match stale.staleness {
        Staleness::MissingJob => format!("{job} does not exist"),
        Staleness::Reclaimed => {
            let epoch = stale.current_epoch.unwrap_or_default();
            match &stale.current_holder {
                Some(holder) => format!("{job} is at epoch {epoch}, held by {}", holder.as_str()),
                None => format!("{job} is at epoch {epoch}"),
            }
        }
        Staleness::NotRunning => format!("{job} is not running"),
        Staleness::Expired => format!("the lease on {job} expired"),
    }
}

/// The error for a refused fence.
pub fn lease_lost(stale: StaleFence) -> Error {
    Error::new(ErrorCode::LeaseLost, stale.to_string()).with_source(stale)
}

/// The `type` tag of an event payload, as the event schema spells it.
fn payload_type(payload: &EventPayload) -> String {
    serde_json::to_value(payload)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fence(holder: &str, epoch: u64) -> FenceToken {
        FenceToken::issue_for_claim(
            JobId::try_from("job-1").expect("valid id"),
            Identifier::try_from(holder).expect("valid id"),
            epoch,
        )
    }

    fn payload(value: serde_json::Value) -> EventPayload {
        serde_json::from_value(value).expect("valid payload")
    }

    fn cancel() -> EventPayload {
        payload(json!({ "type": "cancel_requested" }))
    }

    fn answered() -> EventPayload {
        payload(
            json!({ "type": "approval_answered", "approval_id": "appr-1", "answer": "approve" }),
        )
    }

    fn started() -> EventPayload {
        payload(json!({ "type": "run_started", "task_id": "task-1", "agent_id": "test_agent" }))
    }

    fn checkpoint() -> serde_json::Value {
        json!({ "version": 2, "sha256": format!("sha256:{}", "b".repeat(64)) })
    }

    /// `job_state_changed` from `from` (absent if `None`) into `to`.
    fn changed(from: Option<&str>, to: serde_json::Value) -> EventPayload {
        let mut value = json!({ "type": "job_state_changed", "to": to });
        if let Some(from) = from {
            value["from"] = json!(from);
        }
        payload(value)
    }

    /// Every `job_state_changed` the matrix tries, by name.
    fn state_changes() -> Vec<(&'static str, EventPayload)> {
        vec![
            ("submitted", changed(None, json!({ "state": "queued" }))),
            (
                "submitted_on_a_checkpoint",
                changed(
                    None,
                    json!({ "state": "queued", "checkpoint": checkpoint() }),
                ),
            ),
            (
                "woken",
                changed(
                    Some("suspended"),
                    json!({ "state": "queued", "checkpoint": checkpoint() }),
                ),
            ),
            (
                "woken_from_waiting",
                changed(
                    Some("waiting"),
                    json!({ "state": "queued", "checkpoint": checkpoint() }),
                ),
            ),
            (
                "woken_without_a_checkpoint",
                changed(Some("suspended"), json!({ "state": "queued" })),
            ),
            (
                "requeued",
                changed(
                    Some("running"),
                    json!({ "state": "queued", "checkpoint": checkpoint() }),
                ),
            ),
            (
                "claimed",
                changed(Some("queued"), json!({ "state": "running" })),
            ),
            (
                "completed",
                changed(
                    Some("running"),
                    json!({ "state": "completed", "stop_reason": "success", "checkpoint": checkpoint() }),
                ),
            ),
        ]
    }

    #[test]
    fn a_fence_authorises_every_write_and_is_returned_for_checking() {
        let authority = WriteAuthority::Fenced(fence("worker-1", 3));
        let (v0, v1, event) = (SdrVersion(0), SdrVersion(1), started());
        for write in [
            StoreWrite::JobInsert,
            StoreWrite::JobWake,
            StoreWrite::Event(&event),
            StoreWrite::StepBegin,
            StoreWrite::StepComplete,
            StoreWrite::SdrVersion(&v0),
            StoreWrite::SdrVersion(&v1),
        ] {
            let checked = authorize(Some(&authority), write).expect("a fence is authority");
            assert_eq!(checked.map(FenceToken::epoch), Some(3), "{write}");
        }
    }

    #[test]
    fn each_unfenced_reason_permits_only_its_writes() {
        let (v0, v1) = (SdrVersion(0), SdrVersion(1));
        let (cancel, answered, started) = (cancel(), answered(), started());
        let changes = state_changes();
        let mut writes = vec![
            ("insert", StoreWrite::JobInsert),
            ("wake", StoreWrite::JobWake),
            ("cancel", StoreWrite::Event(&cancel)),
            ("answered", StoreWrite::Event(&answered)),
            ("started", StoreWrite::Event(&started)),
            ("begin", StoreWrite::StepBegin),
            ("complete", StoreWrite::StepComplete),
            ("v0", StoreWrite::SdrVersion(&v0)),
            ("v1", StoreWrite::SdrVersion(&v1)),
        ];
        writes.extend(
            changes
                .iter()
                .map(|(name, payload)| (*name, StoreWrite::Event(payload))),
        );
        let permitted = |reason| match reason {
            UnfencedReason::JobSubmit => vec!["insert", "submitted"],
            UnfencedReason::JobWake => vec!["wake", "woken", "woken_from_waiting"],
            UnfencedReason::ApiCommand => vec!["cancel", "answered"],
            UnfencedReason::ProjectCreate => vec!["v0"],
        };
        for reason in UnfencedReason::ALL {
            let authority = WriteAuthority::Unfenced(reason);
            for &(name, write) in &writes {
                let result = authorize(Some(&authority), write);
                if permitted(reason).contains(&name) {
                    assert!(matches!(result, Ok(None)), "{reason} should permit {name}");
                } else {
                    assert_eq!(
                        result.map_err(|e| e.code()).err(),
                        Some(ErrorCode::LeaseLost),
                        "{reason} should refuse {name}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_missing_authority_permits_nothing() {
        let (v0, event) = (SdrVersion(0), cancel());
        for write in [
            StoreWrite::JobInsert,
            StoreWrite::JobWake,
            StoreWrite::Event(&event),
            StoreWrite::StepBegin,
            StoreWrite::StepComplete,
            StoreWrite::SdrVersion(&v0),
        ] {
            let err = authorize(None, write).expect_err("no authority");
            assert_eq!(err.code(), ErrorCode::LeaseLost, "{write}");
            assert!(err.message().contains("needs a write authority"), "{err}");
        }
    }

    /// Each operation records only its own transition: a submit cannot claim
    /// to have woken a job, nor a wake-up to have submitted one.
    #[test]
    fn submit_and_wake_cannot_record_each_others_transition() {
        let changes = state_changes();
        let change = |name: &str| {
            changes
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, payload)| payload)
                .expect("a named change")
        };
        let submit = WriteAuthority::Unfenced(UnfencedReason::JobSubmit);
        let wake = WriteAuthority::Unfenced(UnfencedReason::JobWake);
        let refused = |authority: &WriteAuthority, name: &str| {
            authorize(Some(authority), StoreWrite::Event(change(name)))
                .map_err(|e| e.code())
                .err()
        };
        assert_eq!(refused(&submit, "woken"), Some(ErrorCode::LeaseLost));
        assert_eq!(
            refused(&submit, "woken_from_waiting"),
            Some(ErrorCode::LeaseLost)
        );
        assert_eq!(refused(&wake, "submitted"), Some(ErrorCode::LeaseLost));
        assert_eq!(refused(&submit, "submitted"), None);
        assert_eq!(refused(&wake, "woken"), None);
    }

    #[test]
    fn a_refused_reason_names_the_reason_and_the_write() {
        let authority = WriteAuthority::Unfenced(UnfencedReason::ApiCommand);
        let event = started();
        let err = authorize(Some(&authority), StoreWrite::Event(&event)).expect_err("refused");
        assert_eq!(
            err.message(),
            "unfenced reason api_command does not permit an event of type run_started"
        );
    }

    #[test]
    fn a_reclaimed_fence_names_both_epochs_and_the_new_holder() {
        let stale = StaleFence::new(
            &fence("worker-1", 3),
            Some(FoundLease {
                epoch: 4,
                holder: Some(Identifier::try_from("worker-2").expect("valid id")),
                running: true,
            }),
        );
        assert_eq!(stale.staleness, Staleness::Reclaimed);
        assert_eq!((stale.fence.epoch(), stale.current_epoch), (3, Some(4)));
        let err = lease_lost(stale);
        assert_eq!(err.code(), ErrorCode::LeaseLost);
        assert_eq!(
            err.message(),
            "fence worker-1@3 on job-1 refused: job-1 is at epoch 4, held by worker-2"
        );
        let source = std::error::Error::source(&err).expect("the source is kept");
        assert!(source.downcast_ref::<StaleFence>().is_some());
    }

    #[test]
    fn the_epoch_decides_before_the_holder_or_the_state() {
        let worker = |name: &str| Some(Identifier::try_from(name).expect("valid id"));
        let found = |epoch, holder, running| FoundLease {
            epoch,
            holder,
            running,
        };
        let cases = [
            (None, Staleness::MissingJob),
            (
                Some(found(4, worker("worker-1"), true)),
                Staleness::Reclaimed,
            ),
            (Some(found(4, None, false)), Staleness::Reclaimed),
            (Some(found(3, None, false)), Staleness::NotRunning),
            (Some(found(3, worker("worker-1"), true)), Staleness::Expired),
        ];
        for (found, expected) in cases {
            let stale = StaleFence::new(&fence("worker-1", 3), found.clone());
            assert_eq!(stale.staleness, expected, "{found:?}");
        }
        let expired = StaleFence::new(
            &fence("worker-1", 3),
            Some(found(3, worker("worker-1"), true)),
        );
        assert_eq!(
            expired.to_string(),
            "fence worker-1@3 on job-1 refused: the lease on job-1 expired"
        );
    }
}
