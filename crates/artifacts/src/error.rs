use std::time::Duration;

use contracts::types::JobState;
use contracts::{Error, ErrorCode, ExecutionContext};

/// What can go wrong describing an artifact or assembling a run report.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactsError {
    #[error("run {run} has no job")]
    UnknownRun { run: String },

    /// Only a stopped run has a report: one that ended, or one suspended on
    /// an approval, whose report is not final.
    #[error("run {run} has not stopped: its job is {state}")]
    NotStopped { run: String, state: JobState },

    #[error("invalid artifact metadata: {}", .problems.join("; "))]
    InvalidMetadata { problems: Vec<String> },

    #[error("{operation} failed: {source}")]
    Store {
        operation: &'static str,
        #[source]
        source: Error,
    },

    #[error("{operation} did not finish within {limit:?}")]
    Timeout {
        operation: &'static str,
        limit: Duration,
    },

    #[error("report assembly was cancelled")]
    Cancelled,

    /// Stored records that contradict each other, or a report that breaks
    /// its own schema. Such a report is never returned.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ArtifactsError {
    /// The code of the contract error this maps to. A store's error keeps
    /// its own code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownRun { .. } => ErrorCode::NotFound,
            Self::NotStopped { .. } => ErrorCode::Conflict,
            Self::InvalidMetadata { .. } => ErrorCode::InvalidInput,
            Self::Store { source, .. } => source.code(),
            Self::Timeout { .. } => ErrorCode::Timeout,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// The contract error with `ctx`'s correlation id attached.
    pub fn into_error(self, ctx: &ExecutionContext) -> Error {
        Error::from(self).with_correlation_id(ctx.correlation_id().clone())
    }
}

impl From<ArtifactsError> for Error {
    fn from(err: ArtifactsError) -> Self {
        Error::new(err.code(), err.to_string()).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_the_documented_table() {
        let cases = [
            (
                ArtifactsError::UnknownRun { run: "r".into() },
                ErrorCode::NotFound,
            ),
            (
                ArtifactsError::NotStopped {
                    run: "r".into(),
                    state: JobState::Running,
                },
                ErrorCode::Conflict,
            ),
            (
                ArtifactsError::InvalidMetadata { problems: vec![] },
                ErrorCode::InvalidInput,
            ),
            (
                ArtifactsError::Store {
                    operation: "reading",
                    source: Error::new(ErrorCode::Unavailable, "down"),
                },
                ErrorCode::Unavailable,
            ),
            (
                ArtifactsError::Timeout {
                    operation: "reading",
                    limit: Duration::from_secs(1),
                },
                ErrorCode::Timeout,
            ),
            (ArtifactsError::Cancelled, ErrorCode::Cancelled),
            (ArtifactsError::Internal("x".into()), ErrorCode::Internal),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
        }
    }
}
