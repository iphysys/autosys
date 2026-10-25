//! The run report from in-memory stores (ADR-0033): statuses and links from
//! the final version's evidence, values from the job and the log, and which
//! runs have a report at all.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::sync::Arc;
use std::time::Duration;

use artifacts::{ASSEMBLER_VERSION, ArtifactsError, ReportAssembler, ReportSources};
use contracts::ids::{ProjectId, RunId, SdrVersion};
use contracts::types::{
    CompletedStopReason, Event, EventPayload, Evidence, EvidenceBody, IgnoredReason, Job, JobState,
    JobStatus, Operation, RequirementStatus, RunReport, StopReason, Transition, UnverifiedReason,
    Usage, WakeCondition,
};
use contracts::{
    ArtifactStore, Clock, ErrorCode, EventLog, EvidenceStore, ExecutionContext, JobStore, JobWrite,
    RecordWrite, SchemaSet, provenance,
};
use sdr::{CommitInfo, InitialVersion, SdrManager, Validation};
use serde_json::{Value, json};
use testkit::builders::{artifact, checkpoint, continuation, id, new_event, parse};
use testkit::clock::ManualClock;
use testkit::leases::{LeasedJob, api_command};
use testkit::stores::{
    InMemoryArtifactStore, InMemoryEventLog, InMemoryEvidenceStore, fenced_sdr_storage,
};

const RUN: &str = "run-1";
const PROJECT: &str = "test-project";

struct Fixture {
    leased: LeasedJob,
    clock: Arc<ManualClock>,
    events: Arc<InMemoryEventLog>,
    evidence: Arc<InMemoryEvidenceStore>,
    artifacts: Arc<InMemoryArtifactStore>,
    sdr: Arc<SdrManager>,
    ctx: ExecutionContext,
}

fn harness() -> Value {
    json!({ "harness_build": "test-build", "model_ids": [], "prompts": [], "skills": [],
        "agent_specs": [] })
}

fn check(evidence_id: &str, subject: &str, verdict: &str) -> Evidence {
    parse(json!({
        "schema_version": "0.1", "id": evidence_id, "run_id": RUN,
        "created_at": "2026-10-26T09:00:00Z", "sdr_version": 0, "subject_ids": [subject],
        "body": { "kind": "check", "check_id": "requirement.numeric_threshold",
            "check_version": "0.1.0", "method": "deterministic", "inputs": [],
            "verdict": verdict }
    }))
}

fn operations() -> Vec<Operation> {
    parse(json!([
        { "op": "add_requirement", "requirement": {
            "id": "R-mass", "text": "Mass at most 1 kg.", "priority": "must",
            "numeric": { "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": 1.0, "unit": "kg" } } },
            "evidence_ids": [] } },
        { "op": "add_requirement", "requirement": {
            "id": "R-finish", "text": "A smooth finish.", "priority": "should",
            "evidence_ids": [] } },
        { "op": "set_parameter", "parameter": {
            "id": "part.mass", "value": { "value": 0.84, "unit": "kg" },
            "source": { "kind": "user" }, "evidence_ids": [] } },
        { "op": "add_decision", "decision": {
            "id": "D-plate", "choice": "A flat plate.", "rationale": "It is the lightest.",
            "related_ids": ["R-mass"] } },
        { "op": "add_assumption", "assumption": {
            "id": "A-alu", "statement": "Aluminium 6061.", "related_ids": [] } }
    ]))
}

impl Fixture {
    /// A project at version 1, committed with check evidence for both
    /// requirements, and run-1 recorded up to its stop.
    async fn new() -> Self {
        Self::committed_with(&["ev-mass", "ev-finish"]).await
    }

    /// Like [`Fixture::new`], with version 1 committed on `evidence` only.
    async fn committed_with(evidence: &[&str]) -> Self {
        let clock = Arc::new(ManualClock::at_epoch());
        let leased = LeasedJob::new(clock.clone(), RUN);
        let sdr = Arc::new(
            SdrManager::new(fenced_sdr_storage(leased.leases()), clock.clone())
                .expect("the SDR schemas compile"),
        );
        let ctx = leased
            .ctx("corr-report")
            .with_project(id(PROJECT))
            .with_run(id(RUN));
        let f = Self {
            events: Arc::new(InMemoryEventLog::fenced_by(leased.leases())),
            evidence: Arc::new(InMemoryEvidenceStore::new()),
            artifacts: Arc::new(InMemoryArtifactStore::new()),
            leased,
            clock,
            sdr,
            ctx,
        };
        f.sdr
            .create_project(
                &id(PROJECT),
                InitialVersion {
                    transition_id: id("tr-init"),
                    actor: parse(json!({ "kind": "system", "component": "test" })),
                    reason: parse(json!("Start the design.")),
                    harness: parse(harness()),
                    requirements: Vec::new(),
                },
                &f.ctx,
            )
            .await
            .expect("create the project");
        for evidence in [
            check("ev-mass", "R-mass", "pass"),
            check("ev-finish", "R-finish", "unverified"),
        ] {
            f.evidence.put(&evidence).await.expect("store evidence");
        }
        f.commit(evidence).await;

        f.append(json!({ "type": "job_state_changed", "from": "queued",
            "to": { "state": "running" }, "harness_build": "build-a" }))
            .await;
        f.append(json!({ "type": "run_started", "task_id": "task-job-1",
            "agent_id": "main_orchestrator" }))
            .await;
        f.append(json!({ "type": "context_assembled",
            "context_hash": format!("sha256:{}", "c".repeat(64)), "total_tokens": 100,
            "sdr_version": 0, "last_event_seq": 2,
            "prompts": [{ "id": "orchestrator.main", "sha256": format!("sha256:{}", "a".repeat(64)) }],
            "agent_spec": { "id": "main_orchestrator", "sha256": format!("sha256:{}", "e".repeat(64)) } }))
            .await;
        f.append(
            json!({ "type": "model_call_completed", "model_profile": "default",
            "model_id": "m-1", "usage": { "input_tokens": 90, "output_tokens": 10 } }),
        )
        .await;
        f.append(
            json!({ "type": "approval_requested", "approval_id": "appr-1",
            "gate": "approval", "subject_id": "tr-1", "summary": "Commit the final design." }),
        )
        .await;
        let mut answer = new_event(
            RUN,
            f.clock.now(),
            json!({ "type": "approval_answered", "approval_id": "appr-1", "answer": "approve" }),
        );
        answer.actor = parse(json!({ "kind": "user", "user_id": "dev" }));
        f.events
            .append(answer, Some(&api_command()))
            .await
            .expect("answer the approval");
        f.append(json!({ "type": "run_stopped", "stop_reason": "success", "sdr_version": 1 }))
            .await;

        for (name, version) in [("art-1", 1), ("art-later", 2)] {
            let mut metadata = artifact(name, RUN, f.clock.now(), b"part");
            metadata.source_sdr_version = SdrVersion(version);
            f.artifacts
                .put(&metadata, b"part")
                .await
                .expect("store an artifact");
        }
        f
    }

    async fn commit(&self, evidence: &[&str]) {
        let transition: Transition = parse(json!({
            "schema_version": "0.1", "id": "tr-1", "base_version": 0,
            "actor": { "kind": "agent", "agent_id": "main_orchestrator" },
            "reason": "The design.", "final": true, "operations": operations()
        }));
        let proposed = self.sdr.propose(id(PROJECT), transition);
        let Validation::Valid(validated) = self
            .sdr
            .validate(proposed, &self.ctx)
            .await
            .expect("validation runs")
        else {
            panic!("the transition is invalid");
        };
        self.sdr
            .commit(
                validated,
                CommitInfo {
                    evidence_ids: evidence.iter().map(|e| id(e)).collect(),
                    harness: parse(harness()),
                },
                &self.ctx,
            )
            .await
            .expect("commit");
    }

    async fn append(&self, payload: Value) -> Event {
        self.clock.advance(Duration::from_secs(1));
        self.events
            .append(
                new_event(RUN, self.clock.now(), payload),
                self.ctx.write_authority(),
            )
            .await
            .expect("append")
    }

    /// Records the move into `status` and saves the job in it, with a record
    /// whose head is version 1.
    async fn stop(&self, status: JobStatus) -> Job {
        let into = contracts::job_state(&status);
        self.append(json!({ "type": "job_state_changed", "from": "running",
            "to": serde_json::to_value(&status).expect("a status") }))
            .await;
        let mut job = self.job().await;
        job.status = status;
        job.budget_used = parse::<Usage>(json!({ "input_tokens": 90, "output_tokens": 10,
            "tool_calls": 3, "steps": 1, "wall_time_seconds": 12.5 }));
        job.progress.last_event_seq = self.events.last_seq(&id(RUN)).await.expect("last seq");
        let mut record = continuation(RUN, 2);
        record.head = Some(SdrVersion(1));
        let jobs: &dyn JobStore = self.leased.jobs().as_ref();
        jobs.save(
            &JobWrite {
                job: &job,
                continuation: RecordWrite::Replace(&record),
                retry_after: None,
            },
            &self.leased.claimed().fence,
        )
        .await
        .expect("save the stopped job");
        assert_eq!(contracts::job_state(&self.job().await.status), into);
        self.job().await
    }

    async fn job(&self) -> Job {
        let jobs: &dyn JobStore = self.leased.jobs().as_ref();
        jobs.get(&id("job-1")).await.expect("job-1")
    }

    fn assembler(&self) -> ReportAssembler {
        ReportAssembler::new(
            ReportSources {
                jobs: self.leased.jobs().clone(),
                events: self.events.clone(),
                evidence: self.evidence.clone(),
                artifacts: self.artifacts.clone(),
                sdr: self.sdr.clone(),
            },
            Duration::from_secs(5),
        )
        .expect("the report schema compiles")
    }

    async fn report(&self) -> Result<RunReport, ArtifactsError> {
        self.assembler()
            .assemble(&id::<RunId>(RUN), &self.ctx)
            .await
    }

    async fn all_events(&self) -> Vec<Event> {
        self.events.read(&id(RUN), 0, 10_000).await.expect("events")
    }
}

fn completed() -> JobStatus {
    JobStatus::Completed {
        stop_reason: CompletedStopReason::Success,
        checkpoint: checkpoint(2),
    }
}

#[tokio::test]
async fn a_completed_run_reports_what_its_records_hold() {
    let f = Fixture::new().await;
    let job = f.stop(completed()).await;
    let report = f.report().await.expect("a report");

    let schemas = SchemaSet::compile(&["reports/run-report.schema.json"]).expect("schema");
    assert_eq!(
        schemas
            .errors_of("reports/run-report.schema.json", &report)
            .expect("validates"),
        Vec::<String>::new()
    );
    assert_eq!(report.assembler_version.as_str(), ASSEMBLER_VERSION);
    assert!(report.final_);
    assert_eq!(report.stop_reason, StopReason::Success);
    assert_eq!(report.final_sdr_version.0, 1);
    assert_eq!(report.objective, job.task.goal);
    assert_eq!(report.project_id, id::<ProjectId>(PROJECT));

    let rows: Vec<(&str, RequirementStatus, Option<UnverifiedReason>)> = report
        .requirements
        .iter()
        .map(|r| (r.requirement_id.as_str(), r.status, r.reason))
        .collect();
    assert_eq!(
        rows,
        [
            ("R-mass", RequirementStatus::Pass, None),
            (
                "R-finish",
                RequirementStatus::Unverified,
                Some(UnverifiedReason::Undecided)
            )
        ]
    );

    let decisions: Vec<&str> = report
        .decisions
        .iter()
        .map(|d| d.decision_id.as_str())
        .collect();
    assert_eq!(decisions, ["D-plate"]);
    let assumptions: Vec<&str> = report
        .assumptions
        .iter()
        .map(|a| a.assumption_id.as_str())
        .collect();
    assert_eq!(assumptions, ["A-alu"]);

    assert_eq!(report.approvals.len(), 1);
    assert_eq!(report.approvals[0].gate.as_str(), "approval");
    assert_eq!(
        serde_json::to_value(&report.approvals[0].actor).expect("actor"),
        json!({ "kind": "user", "user_id": "dev" })
    );
    assert_eq!(report.budget_used.len(), 1);
    assert_eq!(report.budget_used[0].agent_id, job.task.agent_id);
    assert_eq!(report.budget_used[0].usage, job.budget_used);

    let events = f.all_events().await;
    assert_eq!(report.versions, provenance::fold(&events));
    let builds: Vec<&str> = report
        .versions
        .harness_builds
        .iter()
        .map(|b| b.as_str())
        .collect();
    assert_eq!(builds, ["build-a"]);
    assert_eq!(
        (
            report.versions.prompts.len(),
            report.versions.agent_specs.len()
        ),
        (1, 1)
    );
    let started = events
        .iter()
        .find(|e| matches!(e.payload, EventPayload::RunStarted { .. }))
        .expect("run_started");
    assert_eq!(report.started_at, started.occurred_at);
    let ended = events.last().expect("the completed record");
    assert_eq!(report.ended_at, ended.occurred_at);

    let artifacts: Vec<&str> = report
        .artifacts
        .iter()
        .map(|a| a.artifact_id.as_str())
        .collect();
    assert_eq!(
        artifacts,
        ["art-1"],
        "artifacts after the final version are left out"
    );
    assert!(report.narrative.is_none());
    assert!(report.escalations.is_empty());

    assert_eq!(
        f.report().await.expect("again"),
        report,
        "the same records, the same report"
    );
}

#[tokio::test]
async fn every_status_links_to_stored_evidence_with_the_same_verdict() {
    let f = Fixture::new().await;
    f.stop(completed()).await;
    let report = f.report().await.expect("a report");
    for row in &report.requirements {
        assert!(
            !row.evidence.is_empty(),
            "{} links to nothing",
            row.requirement_id.as_str()
        );
        for finding in &row.evidence {
            let stored = f
                .evidence
                .get(&finding.evidence_id)
                .await
                .expect("the linked evidence is stored");
            assert!(stored.subject_ids.contains(&row.requirement_id));
            let EvidenceBody::Check { verdict, .. } = stored.body else {
                panic!("not a check");
            };
            assert_eq!(finding.verdict, verdict);
            assert!(finding.used);
            assert_eq!(finding.ignored_reason, None::<IgnoredReason>);
        }
    }
}

#[tokio::test]
async fn a_suspended_run_has_a_report_that_is_not_final() {
    let f = Fixture::new().await;
    let wake: WakeCondition =
        parse(json!({ "kind": "approval_answered", "approval_id": "appr-2" }));
    f.stop(JobStatus::Suspended {
        wake_condition: wake,
        checkpoint: checkpoint(2),
    })
    .await;
    let report = f.report().await.expect("a report");
    assert!(!report.final_);
    assert_eq!(report.stop_reason, StopReason::ApprovalRequired);
}

#[tokio::test]
async fn a_failed_or_cancelled_run_reports_its_stop() {
    let f = Fixture::new().await;
    f.stop(JobStatus::Cancelled {
        checkpoint: checkpoint(2),
    })
    .await;
    let report = f.report().await.expect("a report");
    assert!(report.final_);
    assert_eq!(report.stop_reason, StopReason::Cancelled);
}

#[tokio::test]
async fn a_run_that_has_not_stopped_or_does_not_exist_has_no_report() {
    let f = Fixture::new().await;
    let running = f.report().await.expect_err("still running");
    assert!(
        matches!(
            &running,
            ArtifactsError::NotStopped {
                state: JobState::Running,
                ..
            }
        ),
        "{running}"
    );
    assert_eq!(running.code(), ErrorCode::Conflict);

    let unknown = f
        .assembler()
        .assemble(&id::<RunId>("run-unknown"), &f.ctx)
        .await
        .expect_err("no such run");
    assert_eq!(unknown.code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn a_requirement_without_findings_is_unverified_with_no_evidence() {
    let f = Fixture::committed_with(&["ev-mass"]).await;
    f.stop(completed()).await;
    let report = f.report().await.expect("a report");
    let finish = report
        .requirements
        .iter()
        .find(|r| r.requirement_id.as_str() == "R-finish")
        .expect("R-finish");
    assert_eq!(finish.status, RequirementStatus::Unverified);
    assert_eq!(finish.reason, Some(UnverifiedReason::NoEvidence));
    assert!(finish.evidence.is_empty());
}
