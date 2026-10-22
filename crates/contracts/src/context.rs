//! What every operation receives: correlation ids, cancellation, and time.

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, ErrorCode};
use crate::ids::{AgentId, CorrelationId, ProjectId, RunId, StepId, TaskId};

/// Correlation ids for tracing and a cancellation token.
///
/// Cloning shares the token. [`ExecutionContext::for_step`] creates a child:
/// cancelling the run cancels its steps, but cancelling a step leaves the run
/// alone.
#[derive(Clone, Debug)]
pub struct ExecutionContext {
    correlation_id: CorrelationId,
    project_id: Option<ProjectId>,
    run_id: Option<RunId>,
    task_id: Option<TaskId>,
    step_id: Option<StepId>,
    agent_id: Option<AgentId>,
    cancellation: CancellationToken,
}

impl ExecutionContext {
    pub fn new(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            project_id: None,
            run_id: None,
            task_id: None,
            step_id: None,
            agent_id: None,
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_project(mut self, id: ProjectId) -> Self {
        self.project_id = Some(id);
        self
    }

    pub fn with_run(mut self, id: RunId) -> Self {
        self.run_id = Some(id);
        self
    }

    pub fn with_task(mut self, id: TaskId) -> Self {
        self.task_id = Some(id);
        self
    }

    pub fn with_agent(mut self, id: AgentId) -> Self {
        self.agent_id = Some(id);
        self
    }

    /// A child context for one step, with a child cancellation token.
    pub fn for_step(&self, id: StepId) -> Self {
        Self {
            step_id: Some(id),
            cancellation: self.cancellation.child_token(),
            ..self.clone()
        }
    }

    pub fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }

    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }

    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    pub fn task_id(&self) -> Option<&TaskId> {
        self.task_id.as_ref()
    }

    pub fn step_id(&self) -> Option<&StepId> {
        self.step_id.as_ref()
    }

    pub fn agent_id(&self) -> Option<&AgentId> {
        self.agent_id.as_ref()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Resolves when this context is cancelled.
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// `Err(Cancelled)` if this context is cancelled, carrying its correlation id.
    pub fn ensure_not_cancelled(&self) -> Result<(), Error> {
        if self.is_cancelled() {
            Err(Error::new(ErrorCode::Cancelled, "operation cancelled")
                .with_correlation_id(self.correlation_id.clone()))
        } else {
            Ok(())
        }
    }
}

/// The source of the current time. Injected so time-dependent behavior, such
/// as lease expiry, is testable and the same in every store implementation.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(CorrelationId::try_from("corr-1").expect("valid id"))
    }

    fn step(id: &str) -> StepId {
        StepId::try_from(id).expect("valid id")
    }

    #[test]
    fn cancelling_the_run_cancels_its_steps() {
        let run = ctx();
        let step = run.for_step(step("step-1"));
        run.cancel();
        assert!(step.is_cancelled());
        assert_eq!(
            step.ensure_not_cancelled().map_err(|e| e.code()),
            Err(ErrorCode::Cancelled)
        );
    }

    #[test]
    fn cancelling_a_step_leaves_the_run_running() {
        let run = ctx();
        let step = run.for_step(step("step-1"));
        step.cancel();
        assert!(!run.is_cancelled());
        assert!(run.ensure_not_cancelled().is_ok());
    }

    #[test]
    fn step_context_keeps_correlation_and_sets_step() {
        let run = ctx().with_run(RunId::try_from("run-1").expect("valid id"));
        let step = run.for_step(step("step-7"));
        assert_eq!(step.correlation_id(), run.correlation_id());
        assert_eq!(step.run_id(), run.run_id());
        assert_eq!(step.step_id().map(|s| s.as_str()), Some("step-7"));
    }

    #[tokio::test]
    async fn cancelled_resolves_after_cancel() {
        let run = ctx();
        let waiter = run.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        run.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("cancelled() resolves")
            .expect("task completes");
    }
}
