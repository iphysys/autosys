use std::path::PathBuf;
use std::time::Duration;

use contracts::types::ApiError;

/// Why a call to the API failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server refused the request and said why.
    #[error(
        "{status} {}: {}{}",
        body.code.as_str(),
        body.message.as_str(),
        correlation(body)
    )]
    Api { status: u16, body: ApiError },
    /// The server answered with an error status but no `ApiError` body.
    #[error("the server answered {status} without an error body")]
    Status { status: u16 },
    /// The server could not be reached, or the connection broke.
    #[error("cannot reach the API: {0}")]
    Transport(String),
    #[error("no answer within {0:?}")]
    Timeout(Duration),
    /// The server answered something API v0 does not describe: a body that
    /// is not its schema's, or events out of order.
    #[error("the answer is not what API v0 describes: {0}")]
    Protocol(String),
    /// The caller passed something the client cannot send.
    #[error("{0}")]
    InvalidInput(String),
    #[error("{}: {reason}", path.display())]
    Journal { path: PathBuf, reason: String },
}

impl ClientError {
    /// Whether the same call may succeed if tried again: the server could
    /// not be reached or said so itself.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Timeout(_) => true,
            Self::Api { body, .. } => body.retryable,
            Self::Status { status } => *status >= 500,
            Self::Protocol(_) | Self::InvalidInput(_) | Self::Journal { .. } => false,
        }
    }
}

fn correlation(body: &ApiError) -> String {
    body.correlation_id
        .as_ref()
        .map(|id| format!(" (correlation id {})", id.as_str()))
        .unwrap_or_default()
}

impl From<reqwest::Error> for ClientError {
    fn from(error: reqwest::Error) -> Self {
        // The error names the URL, never the token, which travels in a
        // header.
        Self::Transport(error.to_string())
    }
}
