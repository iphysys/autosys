//! Errors of the context manager, and how they map onto the contract error.

use std::time::Duration;

use contracts::types::ContextSectionName;
use contracts::{Error, ErrorCode, ExecutionContext};

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("unknown context profile `{name}`; available: {}", .available.join(", "))]
    UnknownProfile {
        name: String,
        available: Vec<String>,
    },

    #[error("unknown prompt `{path}`; available: {}", .available.join(", "))]
    UnknownPrompt {
        path: String,
        available: Vec<String>,
    },

    /// Includes a header whose `inputs` names a section that does not exist.
    #[error("invalid prompt `{path}`: {}", .problems.join("; "))]
    InvalidPrompt { path: String, problems: Vec<String> },

    #[error("invalid context profiles: {}", .problems.join("; "))]
    InvalidProfile { problems: Vec<String> },

    #[error("invalid open turn: {reason}")]
    InvalidOpenTurn { reason: String },

    #[error("the execution context has no run id, so the context cannot be recorded")]
    MissingRun,

    /// A section that cannot be cut does not fit, or everything that can be
    /// cut is gone and the context still does not fit. Nothing is truncated.
    #[error("{section} needs {needed} tokens but at most {limit} are available")]
    Overflow {
        section: ContextSectionName,
        needed: u64,
        limit: u64,
    },

    #[error("reading the SDR failed: {0}")]
    Sdr(#[source] Error),

    #[error("the event log failed: {0}")]
    Events(#[source] Error),

    #[error("{operation} did not finish within {limit:?}")]
    Timeout {
        operation: &'static str,
        limit: Duration,
    },

    #[error("context assembly was cancelled")]
    Cancelled,

    /// A broken invariant of the crate itself.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ContextError {
    /// The code of the contract error this maps to. Errors from the SDR and
    /// the event log keep their own code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownProfile { .. }
            | Self::InvalidPrompt { .. }
            | Self::InvalidProfile { .. }
            | Self::InvalidOpenTurn { .. }
            | Self::MissingRun => ErrorCode::InvalidInput,
            Self::UnknownPrompt { .. } => ErrorCode::NotFound,
            Self::Overflow { .. } => ErrorCode::ContextOverflow,
            Self::Sdr(e) | Self::Events(e) => e.code(),
            Self::Timeout { .. } => ErrorCode::Timeout,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// The contract error with `ctx`'s correlation id attached. The
    /// `From` conversion cannot know the id.
    pub fn into_error(self, ctx: &ExecutionContext) -> Error {
        Error::from(self).with_correlation_id(ctx.correlation_id().clone())
    }
}

impl From<ContextError> for Error {
    fn from(err: ContextError) -> Self {
        Error::new(err.code(), err.to_string()).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(contracts::ids::CorrelationId::try_from("corr-1").expect("valid id"))
    }

    #[test]
    fn codes_follow_the_documented_table() {
        let unavailable = Error::new(ErrorCode::Unavailable, "down");
        let cases = [
            (
                ContextError::UnknownProfile {
                    name: "x".into(),
                    available: vec!["default".into()],
                },
                ErrorCode::InvalidInput,
            ),
            (
                ContextError::UnknownPrompt {
                    path: "x.md".into(),
                    available: vec![],
                },
                ErrorCode::NotFound,
            ),
            (
                ContextError::InvalidPrompt {
                    path: "x.md".into(),
                    problems: vec!["bad".into()],
                },
                ErrorCode::InvalidInput,
            ),
            (
                ContextError::InvalidProfile {
                    problems: vec!["bad".into()],
                },
                ErrorCode::InvalidInput,
            ),
            (
                ContextError::InvalidOpenTurn {
                    reason: "bad".into(),
                },
                ErrorCode::InvalidInput,
            ),
            (ContextError::MissingRun, ErrorCode::InvalidInput),
            (
                ContextError::Overflow {
                    section: ContextSectionName::DesignState,
                    needed: 2,
                    limit: 1,
                },
                ErrorCode::ContextOverflow,
            ),
            (ContextError::Sdr(unavailable), ErrorCode::Unavailable),
            (
                ContextError::Events(Error::new(ErrorCode::Conflict, "x")),
                ErrorCode::Conflict,
            ),
            (
                ContextError::Timeout {
                    operation: "reading the SDR",
                    limit: Duration::from_secs(1),
                },
                ErrorCode::Timeout,
            ),
            (ContextError::Cancelled, ErrorCode::Cancelled),
            (ContextError::Internal("x".into()), ErrorCode::Internal),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "{err}");
            assert_eq!(Error::from(err).code(), code);
        }
    }

    #[test]
    fn the_correlation_id_is_attached_and_the_source_kept() {
        let err = ContextError::Overflow {
            section: ContextSectionName::TaskContract,
            needed: 10,
            limit: 5,
        }
        .into_error(&ctx());
        assert_eq!(err.code(), ErrorCode::ContextOverflow);
        assert_eq!(err.correlation_id().map(|c| c.as_str()), Some("corr-1"),);
        assert!(!err.retryable());
        assert!(err.message().contains("task_contract"), "{}", err.message());
        assert!(std::error::Error::source(&err).is_some());
    }
}
