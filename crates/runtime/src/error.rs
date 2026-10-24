//! Errors of the runtime, and how they map onto the contract error.
//!
//! A `RuntimeError` is a harness failure: something the run cannot recover
//! from by itself, such as an invalid agent or task, or a store that fails.
//! Everything the model or a tool did wrong is an observation or a stop
//! reason instead, never an error (blueprint section 13.1).

use std::time::Duration;

use context::ContextError;
use contracts::{Error, ErrorCode, ExecutionContext};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Agent specs that cannot be registered, with every problem found.
    #[error("invalid agent specs: {}", .problems.join("; "))]
    InvalidAgents { problems: Vec<String> },

    #[error("unknown agent `{id}`; available: {}", .available.join(", "))]
    UnknownAgent { id: String, available: Vec<String> },

    /// A task that breaks its schema or does not fit its agent.
    #[error("task `{task}` cannot run: {}", .problems.join("; "))]
    InvalidTask { task: String, problems: Vec<String> },

    /// A component the run depends on failed: a store, a gateway, the
    /// checks or the gates. The source keeps its own code.
    #[error("{operation} failed: {source}")]
    Component {
        operation: &'static str,
        #[source]
        source: Error,
    },

    #[error("assembling the context failed: {0}")]
    Context(#[source] ContextError),

    #[error("{operation} did not finish within {limit:?}")]
    Timeout {
        operation: &'static str,
        limit: Duration,
    },

    #[error("{operation} was cancelled")]
    Cancelled { operation: &'static str },

    /// A broken invariant of the crate itself.
    #[error("internal error: {0}")]
    Internal(String),
}

impl RuntimeError {
    /// The code of the contract error this maps to. Errors of components
    /// keep their own code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidAgents { .. } | Self::InvalidTask { .. } => ErrorCode::InvalidInput,
            Self::UnknownAgent { .. } => ErrorCode::NotFound,
            Self::Component { source, .. } => source.code(),
            Self::Context(e) => e.code(),
            Self::Timeout { .. } => ErrorCode::Timeout,
            Self::Cancelled { .. } => ErrorCode::Cancelled,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Whether running the same job again may succeed.
    pub fn retryable(&self) -> bool {
        self.code().is_retryable()
    }

    /// The contract error with `ctx`'s correlation id attached.
    pub fn into_error(self, ctx: &ExecutionContext) -> Error {
        Error::from(self).with_correlation_id(ctx.correlation_id().clone())
    }
}

impl From<RuntimeError> for Error {
    fn from(err: RuntimeError) -> Self {
        Error::new(err.code(), err.to_string()).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_the_variant_or_the_source() {
        let cases = [
            (
                RuntimeError::InvalidAgents {
                    problems: vec!["x".into()],
                },
                ErrorCode::InvalidInput,
            ),
            (
                RuntimeError::UnknownAgent {
                    id: "a".into(),
                    available: vec!["b".into()],
                },
                ErrorCode::NotFound,
            ),
            (
                RuntimeError::InvalidTask {
                    task: "t".into(),
                    problems: vec!["x".into()],
                },
                ErrorCode::InvalidInput,
            ),
            (
                RuntimeError::Component {
                    operation: "appending an event",
                    source: Error::new(ErrorCode::Unavailable, "down"),
                },
                ErrorCode::Unavailable,
            ),
            (
                RuntimeError::Timeout {
                    operation: "reading the SDR",
                    limit: Duration::from_secs(1),
                },
                ErrorCode::Timeout,
            ),
            (
                RuntimeError::Cancelled {
                    operation: "reading the SDR",
                },
                ErrorCode::Cancelled,
            ),
            (RuntimeError::Internal("x".into()), ErrorCode::Internal),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "{err}");
            assert_eq!(Error::from(err).code(), code);
        }
    }

    #[test]
    fn the_message_lists_every_problem_and_the_source_is_kept() {
        let err = RuntimeError::InvalidTask {
            task: "task-1".into(),
            problems: vec!["one".into(), "two".into()],
        };
        assert_eq!(err.to_string(), "task `task-1` cannot run: one; two");
        let ctx = ExecutionContext::new("corr-1".parse().expect("id"));
        let contract = err.into_error(&ctx);
        assert_eq!(
            contract.correlation_id().map(|c| c.as_str()),
            Some("corr-1")
        );
        assert!(std::error::Error::source(&contract).is_some());
        assert!(!contract.retryable());
    }
}
