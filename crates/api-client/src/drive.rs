//! One run driven to its end through the API (ADR-0035): what `cargo xtask
//! eval` and its system test share. It makes client calls only: it answers
//! the approvals a run asks for, as the operator would, and collects what a
//! scorer reads. It decides and scores nothing.

use std::collections::BTreeSet;
use std::time::Duration;

use contracts::ids::{ApprovalId, RunId, SdrVersion};
use contracts::types::{
    ApprovalAnswer, ApprovalAnswerRequest, CancelRequest, Event, EventPayload, NonEmptyString, Run,
    RunReport, RunRequest, SdrVersionView,
};
use tokio::time::Instant;

use crate::{Client, ClientError, IdempotencyKey};

/// How a run is driven.
#[derive(Clone, Debug)]
pub struct DriveSettings {
    /// The answer to every approval the run asks for.
    pub answer: ApprovalAnswer,
    pub comment: Option<NonEmptyString>,
    /// How long the run may take before it is cancelled.
    pub deadline: Duration,
    /// How long a cancelled run may take to stop.
    pub grace: Duration,
}

/// What a driven run left: its final state, report, events and every
/// committed version, from 0 to the report's final version.
#[derive(Clone, Debug)]
pub struct Driven {
    pub run: Run,
    pub report: RunReport,
    pub events: Vec<Event>,
    pub versions: Vec<SdrVersionView>,
    /// The approvals this drive answered, in order.
    pub answered: Vec<ApprovalId>,
    /// True when the deadline passed and the drive cancelled the run.
    pub cancelled: bool,
}

/// Why a drive did not finish, with the run if one was started.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct DriveFailure {
    pub run_id: Option<RunId>,
    pub error: ClientError,
}

/// Starts `request` with `key` and drives the run to its end. Each
/// `approval_requested` is answered once with `settings.answer`. After
/// `settings.deadline` the run is cancelled; if it has not stopped within
/// `settings.grace` after that, the drive fails with a timeout.
pub async fn drive(
    client: &Client,
    request: &RunRequest,
    key: &IdempotencyKey,
    settings: &DriveSettings,
) -> Result<Driven, DriveFailure> {
    let started = client
        .start_run(request, key)
        .await
        .map_err(|error| DriveFailure {
            run_id: None,
            error,
        })?;
    let run_id = started.run.run_id;
    let fail = |error: ClientError| DriveFailure {
        run_id: Some(run_id.clone()),
        error,
    };

    let mut events: Vec<Event> = Vec::new();
    let mut answered: Vec<ApprovalId> = Vec::new();
    let mut asked: BTreeSet<String> = BTreeSet::new();
    let mut cancelled = false;
    let mut until = Instant::now() + settings.deadline;
    'stream: loop {
        // A fresh stream after the last event kept, so an interrupted read
        // never loses or repeats one.
        let after = events.last().map_or(0, |e| e.seq.get());
        let mut stream = client.events(&run_id, after);
        loop {
            match tokio::time::timeout_at(until, stream.next()).await {
                Ok(Ok(Some(event))) => {
                    if let EventPayload::ApprovalRequested { approval_id, .. } = &event.payload
                        && asked.insert(approval_id.as_str().to_owned())
                    {
                        answer(client, &run_id, approval_id, settings)
                            .await
                            .map_err(fail)?;
                        answered.push(approval_id.clone());
                    }
                    events.push(event);
                }
                Ok(Ok(None)) => break 'stream,
                Ok(Err(error)) => return Err(fail(error)),
                Err(_) if !cancelled => {
                    cancel(client, &run_id, settings.deadline)
                        .await
                        .map_err(fail)?;
                    cancelled = true;
                    until = Instant::now() + settings.grace;
                    continue 'stream;
                }
                Err(_) => return Err(fail(ClientError::Timeout(settings.grace))),
            }
        }
    }

    let run = client.run(&run_id).await.map_err(fail)?;
    let report = client.report(&run_id).await.map_err(fail)?;
    let mut versions = Vec::new();
    for version in 0..=report.final_sdr_version.0 {
        versions.push(
            client
                .sdr_version(&report.project_id, &SdrVersion(version))
                .await
                .map_err(fail)?,
        );
    }
    Ok(Driven {
        run,
        report,
        events,
        versions,
        answered,
        cancelled,
    })
}

/// Answers `approval_id`. A 409 means it was answered already, or the run
/// stopped meanwhile; the stream shows which.
async fn answer(
    client: &Client,
    run_id: &RunId,
    approval_id: &ApprovalId,
    settings: &DriveSettings,
) -> Result<(), ClientError> {
    let request = ApprovalAnswerRequest {
        answer: settings.answer,
        comment: settings.comment.clone(),
    };
    match client.answer_approval(run_id, approval_id, &request).await {
        Ok(_) | Err(ClientError::Api { status: 409, .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Cancels a run that ran past `deadline`. A 409 means it stopped first.
async fn cancel(client: &Client, run_id: &RunId, deadline: Duration) -> Result<(), ClientError> {
    tracing::warn!(
        run = run_id.as_str(),
        ?deadline,
        "the run passed its deadline; cancelling it"
    );
    let reason = NonEmptyString::try_from(format!(
        "the run did not stop within {} s",
        deadline.as_secs()
    ))
    .ok();
    match client.cancel(run_id, &CancelRequest { reason }).await {
        Ok(_) | Err(ClientError::Api { status: 409, .. }) => Ok(()),
        Err(error) => Err(error),
    }
}
