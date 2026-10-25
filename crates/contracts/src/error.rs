//! The error type shared by every contract trait.

use std::fmt;

use schema_types::{ErrorInfo, NonEmptyString};

use crate::ids::{CorrelationId, Identifier};

/// Stable error codes. The strings from [`ErrorCode::as_str`] are part of the
/// contract: they appear in events and results and are never renamed.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The request failed validation.
    InvalidInput,
    NotFound,
    /// The request conflicts with current state, for example a stale version or a lost lease.
    Conflict,
    PermissionDenied,
    Timeout,
    Cancelled,
    /// A dependency is temporarily unavailable, for example rate-limited or down.
    Unavailable,
    /// The tool ran and reported a failure. Never turned into success.
    ToolFailed,
    /// A model call's context does not fit its budget, or the provider
    /// refused the prompt as too long. Repeating the same request cannot help:
    /// the work has to shrink or the budget change (ADR-0029).
    ContextOverflow,
    /// The write's lease fence is stale: another claim took the job, its lease
    /// expired, or it left running. The worker abandons the run; it never
    /// marks the job failed, retries, or shows the error to the model
    /// (ADR-0032).
    LeaseLost,
    /// A bug or broken invariant.
    Internal,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 11] = [
        ErrorCode::InvalidInput,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::PermissionDenied,
        ErrorCode::Timeout,
        ErrorCode::Cancelled,
        ErrorCode::Unavailable,
        ErrorCode::ToolFailed,
        ErrorCode::ContextOverflow,
        ErrorCode::LeaseLost,
        ErrorCode::Internal,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidInput => "invalid_input",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::ToolFailed => "tool_failed",
            ErrorCode::ContextOverflow => "context_overflow",
            ErrorCode::LeaseLost => "lease_lost",
            ErrorCode::Internal => "internal",
        }
    }

    /// Whether repeating the same request may succeed. Retries must still be
    /// bounded and idempotency-aware (blueprint section 13.1).
    pub const fn is_retryable(self) -> bool {
        matches!(self, ErrorCode::Timeout | ErrorCode::Unavailable)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error with a stable code, a message, a retryable flag derived from the
/// code, an optional correlation id, and the source error it came from.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
    code: ErrorCode,
    message: String,
    correlation_id: Option<CorrelationId>,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// An empty message is replaced by the code, so a message is always present.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let message = message.into();
        let message = if message.is_empty() {
            code.as_str().to_string()
        } else {
            message
        };
        Self {
            code,
            message,
            correlation_id: None,
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn with_correlation_id(mut self, id: CorrelationId) -> Self {
        self.correlation_id = Some(id);
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.code.is_retryable()
    }

    pub fn correlation_id(&self) -> Option<&CorrelationId> {
        self.correlation_id.as_ref()
    }

    /// The serializable form used in events and results.
    pub fn to_info(&self) -> ErrorInfo {
        ErrorInfo {
            code: code_identifier(self.code),
            message: non_empty(&self.message),
            retryable: self.retryable(),
            correlation_id: self.correlation_id.clone(),
        }
    }
}

#[allow(
    clippy::expect_used,
    reason = "every ErrorCode string matches the Identifier pattern; checked by tests::codes_are_identifiers"
)]
fn code_identifier(code: ErrorCode) -> Identifier {
    Identifier::try_from(code.as_str()).expect("error codes are valid identifiers")
}

#[allow(
    clippy::expect_used,
    reason = "Error::new never stores an empty message"
)]
fn non_empty(message: &str) -> NonEmptyString {
    NonEmptyString::try_from(message).expect("error messages are never empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_identifiers() {
        for code in ErrorCode::ALL {
            assert!(Identifier::try_from(code.as_str()).is_ok(), "{code}");
        }
    }

    #[test]
    fn only_timeout_and_unavailable_are_retryable() {
        let retryable: Vec<_> = ErrorCode::ALL
            .into_iter()
            .filter(|c| c.is_retryable())
            .collect();
        assert_eq!(retryable, [ErrorCode::Timeout, ErrorCode::Unavailable]);
    }

    #[test]
    fn context_overflow_has_a_stable_string_and_is_not_retryable() {
        assert_eq!(ErrorCode::ContextOverflow.as_str(), "context_overflow");
        assert!(!ErrorCode::ContextOverflow.is_retryable());
        let info =
            Error::new(ErrorCode::ContextOverflow, "design_state needs 9000 tokens").to_info();
        assert_eq!(info.code.as_str(), "context_overflow");
        assert!(!info.retryable);
    }

    #[test]
    fn lease_lost_has_a_stable_string_and_is_not_retryable() {
        assert_eq!(ErrorCode::LeaseLost.as_str(), "lease_lost");
        assert!(!ErrorCode::LeaseLost.is_retryable());
        let info = Error::new(ErrorCode::LeaseLost, "fence worker-1@3 on job-1 refused").to_info();
        assert_eq!(info.code.as_str(), "lease_lost");
        assert!(!info.retryable);
    }

    #[test]
    fn there_are_eleven_codes() {
        assert_eq!(ErrorCode::ALL.len(), 11);
    }

    #[test]
    fn code_strings_are_unique() {
        let mut strings: Vec<&str> = ErrorCode::ALL.into_iter().map(ErrorCode::as_str).collect();
        strings.sort_unstable();
        strings.dedup();
        assert_eq!(strings.len(), ErrorCode::ALL.len());
    }

    #[test]
    fn empty_message_falls_back_to_code() {
        let err = Error::new(ErrorCode::NotFound, "");
        assert_eq!(err.message(), "not_found");
        assert_eq!(err.to_info().message.as_str(), "not_found");
    }

    #[test]
    fn info_keeps_code_flag_and_correlation() {
        let correlation = CorrelationId::try_from("corr-1").expect("valid id");
        let info = Error::new(ErrorCode::Timeout, "tool took too long")
            .with_correlation_id(correlation.clone())
            .to_info();
        assert_eq!(info.code.as_str(), "timeout");
        assert!(info.retryable);
        assert_eq!(info.correlation_id, Some(correlation));
    }

    #[test]
    fn source_is_kept() {
        let io = std::io::Error::other("disk full");
        let err = Error::new(ErrorCode::Unavailable, "write failed").with_source(io);
        let source = std::error::Error::source(&err).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("disk full"));
    }
}
