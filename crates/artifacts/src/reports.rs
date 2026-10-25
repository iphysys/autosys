//! The run report (blueprint section 18.1, ADR-0033), assembled on demand.
//!
//! Every value comes from a durable record, read as of the run's final SDR
//! version:
//!
//! - the job: objective, stop reason, budget used;
//! - its continuation record: the final SDR version, the latest the run saw
//!   or made;
//! - the SDR at that version: requirements, decisions, assumptions, and the
//!   version record whose evidence the requirement statuses come from, by
//!   the rule the `success` stop uses (`contracts::requirement_status`);
//! - the session log: start and end times, approvals, and the versions of
//!   everything that shaped the run (`contracts::provenance::fold`);
//! - the artifact store: the run's artifacts made at or before that version.
//!
//! A suspended run can still continue, so its report is not final. A report
//! that breaks its schema is a harness bug and is never returned.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use contracts::ids::{ApprovalId, Identifier, RunId};
use contracts::types::{
    AgentUsage, CompletedStopReason, ElementKind, Event, EventPayload, Evidence, Job, JobState,
    JobStatus, ReportApproval, ReportAssumption, ReportDecision, RequirementStatus,
    RequirementStatusRow, RunReport, RunReportSchemaVersion, SdrSlice, StopReason, Timestamp,
};
use contracts::{
    ArtifactStore, ConsideredFinding, Error, ErrorCode, EventLog, EvidenceStore, ExecutionContext,
    JobStore, SchemaSet, job_state, provenance, requirement_status,
};
use sdr::SdrManager;

use crate::error::ArtifactsError;
use crate::metadata::report_row;

/// The version of the assembly code, recorded in every report so each one is
/// traceable to the logic that built it.
pub const ASSEMBLER_VERSION: &str = "0.1.0";

const REPORT: &str = "reports/run-report.schema.json";
/// How many events one read returns at most.
const PAGE: usize = 1_000;

/// The stores a report is read from.
#[derive(Clone)]
pub struct ReportSources {
    pub jobs: Arc<dyn JobStore>,
    pub events: Arc<dyn EventLog>,
    pub evidence: Arc<dyn EvidenceStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub sdr: Arc<SdrManager>,
}

/// Assembles run reports. Reads only; see the module docs.
pub struct ReportAssembler {
    sources: ReportSources,
    io_timeout: Duration,
    schemas: SchemaSet,
}

impl ReportAssembler {
    /// `io_timeout` bounds each store call.
    pub fn new(sources: ReportSources, io_timeout: Duration) -> Result<Self, ArtifactsError> {
        let schemas = SchemaSet::compile(&[REPORT])
            .map_err(|e| ArtifactsError::Internal(format!("the run report schema: {e}")))?;
        Ok(Self {
            sources,
            io_timeout,
            schemas,
        })
    }

    /// The report of `run_id`. UnknownRun without a job; NotStopped while
    /// the job is queued, running or waiting.
    pub async fn assemble(
        &self,
        run_id: &RunId,
        ctx: &ExecutionContext,
    ) -> Result<RunReport, ArtifactsError> {
        let s = &self.sources;
        let job = self
            .bounded(ctx, "reading the job", s.jobs.find_by_run(run_id))
            .await?
            .ok_or_else(|| ArtifactsError::UnknownRun {
                run: run_id.as_str().to_owned(),
            })?;
        let (stop_reason, final_) = stopped(&job)?;
        let record = self
            .bounded(
                ctx,
                "reading the continuation",
                s.jobs.continuation(&job.id),
            )
            .await?
            .ok_or_else(|| {
                ArtifactsError::Internal(format!(
                    "job {} has stopped but holds no continuation record",
                    job.id.as_str()
                ))
            })?;
        let version = record
            .head
            .clone()
            .unwrap_or_else(|| job.sdr_base_version.clone());

        let all_kinds = SdrSlice {
            kinds: vec![
                ElementKind::Requirement,
                ElementKind::Parameter,
                ElementKind::Decision,
                ElementKind::Assumption,
            ],
            ids: Vec::new(),
        };
        let document = self
            .bounded(
                ctx,
                "reading the design",
                s.sdr.read(&job.project_id, &all_kinds, &version, ctx),
            )
            .await?;
        let version_record = self
            .bounded(
                ctx,
                "reading the version record",
                s.sdr.version_record(&job.project_id, &version, ctx),
            )
            .await?;
        let mut evidence: Vec<Evidence> = Vec::new();
        for id in &version_record.evidence_ids {
            evidence.push(
                self.bounded(ctx, "reading evidence", s.evidence.get(id))
                    .await?,
            );
        }
        let findings = ConsideredFinding::all(&evidence);
        let requirements: Vec<RequirementStatusRow> = document
            .requirements
            .iter()
            .map(|r| requirement_status(r, &findings, None))
            .collect();

        let events = self.events(run_id, ctx).await?;
        let artifacts = self
            .bounded(ctx, "listing artifacts", s.artifacts.list_for_run(run_id))
            .await?
            .iter()
            .filter(|a| a.source_sdr_version.0 <= version.0)
            .map(report_row)
            .collect();

        let report = RunReport {
            schema_version: RunReportSchemaVersion::X01,
            assembler_version: ASSEMBLER_VERSION
                .parse()
                .map_err(|_| ArtifactsError::Internal("the assembler version is invalid".into()))?,
            final_,
            run_id: job.run_id.clone(),
            project_id: job.project_id.clone(),
            objective: job.task.goal.clone(),
            started_at: started_at(&events).unwrap_or_else(|| job.created_at.clone()),
            ended_at: ended_at(&events, job_state(&job.status))
                .unwrap_or_else(|| job.updated_at.clone()),
            stop_reason,
            final_sdr_version: version.clone(),
            requirements,
            decisions: document
                .design
                .decisions
                .iter()
                .map(|d| ReportDecision {
                    decision_id: d.id.clone(),
                    choice: d.choice.clone(),
                    rationale: d.rationale.clone(),
                })
                .collect(),
            assumptions: document
                .design
                .assumptions
                .iter()
                .map(|a| ReportAssumption {
                    assumption_id: a.id.clone(),
                    statement: a.statement.clone(),
                })
                .collect(),
            artifacts,
            budget_used: vec![AgentUsage {
                agent_id: job.task.agent_id.clone(),
                usage: job.budget_used.clone(),
            }],
            approvals: approvals(&events)?,
            // Nothing escalates until delegation (ADR-0030).
            escalations: Vec::new(),
            versions: provenance::fold(&events),
            // No narrative until the finish call carries one (ADR-0033).
            narrative: None,
        };
        self.check(&report)?;
        tracing::debug!(
            run_id = run_id.as_str(),
            final_sdr_version = version.0,
            final_ = final_,
            "run report assembled"
        );
        Ok(report)
    }

    /// Every event of `run_id`, in order.
    async fn events(
        &self,
        run_id: &RunId,
        ctx: &ExecutionContext,
    ) -> Result<Vec<Event>, ArtifactsError> {
        let mut events: Vec<Event> = Vec::new();
        loop {
            let after = events.last().map_or(0, |e| e.seq.get());
            let page = self
                .bounded(
                    ctx,
                    "reading the run's events",
                    self.sources.events.read(run_id, after, PAGE),
                )
                .await?;
            let full = page.len() == PAGE;
            events.extend(page);
            if !full {
                return Ok(events);
            }
        }
    }

    /// The report against its schema, plus the rules the schema cannot
    /// state: a reason exactly when a status is unverified, and an ignored
    /// reason exactly when a finding is not used.
    fn check(&self, report: &RunReport) -> Result<(), ArtifactsError> {
        let mut problems = self
            .schemas
            .errors_of(REPORT, report)
            .map_err(|e| ArtifactsError::Internal(format!("validating the report: {e}")))?;
        for row in &report.requirements {
            let unverified = row.status == RequirementStatus::Unverified;
            if unverified != row.reason.is_some() {
                problems.push(format!(
                    "requirement {}: status {} with reason {:?}",
                    row.requirement_id.as_str(),
                    row.status,
                    row.reason
                ));
            }
            for finding in &row.evidence {
                if finding.used == finding.ignored_reason.is_some() {
                    problems.push(format!(
                        "requirement {}: finding {} used {} with ignored reason {:?}",
                        row.requirement_id.as_str(),
                        finding.evidence_id.as_str(),
                        finding.used,
                        finding.ignored_reason
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ArtifactsError::Internal(format!(
                "the assembled report is invalid: {}",
                problems.join("; ")
            )))
        }
    }

    /// Runs `future` until it finishes, the I/O timeout passes, or `ctx` is
    /// cancelled.
    async fn bounded<T>(
        &self,
        ctx: &ExecutionContext,
        operation: &'static str,
        future: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, ArtifactsError> {
        let limit = self.io_timeout;
        tokio::select! {
            biased;
            () = ctx.cancelled() => Err(ArtifactsError::Cancelled),
            finished = tokio::time::timeout(limit, future) => match finished {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(e)) if e.code() == ErrorCode::Cancelled => Err(ArtifactsError::Cancelled),
                Ok(Err(source)) => Err(ArtifactsError::Store { operation, source }),
                Err(_) => Err(ArtifactsError::Timeout { operation, limit }),
            },
        }
    }
}

/// The stop reason of a stopped job, and whether its report is final.
fn stopped(job: &Job) -> Result<(StopReason, bool), ArtifactsError> {
    match &job.status {
        JobStatus::Completed { stop_reason, .. } => Ok((
            match stop_reason {
                CompletedStopReason::Success => StopReason::Success,
                CompletedStopReason::Failure => StopReason::Failure,
                CompletedStopReason::BudgetExhausted => StopReason::BudgetExhausted,
            },
            true,
        )),
        JobStatus::Cancelled { .. } => Ok((StopReason::Cancelled, true)),
        JobStatus::Failed { .. } => Ok((StopReason::Failure, true)),
        JobStatus::Suspended { .. } => Ok((StopReason::ApprovalRequired, false)),
        status @ (JobStatus::Queued { .. } | JobStatus::Running | JobStatus::Waiting { .. }) => {
            Err(ArtifactsError::NotStopped {
                run: job.run_id.as_str().to_owned(),
                state: job_state(status),
            })
        }
    }
}

/// When the run first started.
fn started_at(events: &[Event]) -> Option<Timestamp> {
    events
        .iter()
        .find(|e| matches!(e.payload, EventPayload::RunStarted { .. }))
        .map(|e| e.occurred_at.clone())
}

/// When the job last moved into `state`, the state it is in.
fn ended_at(events: &[Event], state: JobState) -> Option<Timestamp> {
    events
        .iter()
        .rev()
        .find(|e| matches!(&e.payload, EventPayload::JobStateChanged { to, .. } if job_state(to) == state))
        .map(|e| e.occurred_at.clone())
}

/// Each answer, with the gate that asked. The answering human is the
/// event's actor.
fn approvals(events: &[Event]) -> Result<Vec<ReportApproval>, ArtifactsError> {
    let gates: HashMap<&ApprovalId, &Identifier> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ApprovalRequested {
                approval_id, gate, ..
            } => Some((approval_id, gate)),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ApprovalAnswered {
                approval_id,
                answer,
                ..
            } => Some((e, approval_id, answer)),
            _ => None,
        })
        .map(|(event, approval_id, answer)| {
            let gate = gates.get(approval_id).ok_or_else(|| {
                ArtifactsError::Internal(format!(
                    "approval {} was answered but never requested",
                    approval_id.as_str()
                ))
            })?;
            Ok(ReportApproval {
                approval_id: approval_id.clone(),
                gate: (*gate).clone(),
                answer: *answer,
                actor: event.actor.clone(),
                answered_at: event.occurred_at.clone(),
            })
        })
        .collect()
}
