//! Error responses: the contract error as an HTTP status and an `ApiError`
//! body (`schemas/api/error.schema.json`).

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use contracts::ids::CorrelationId;
use contracts::{Error, ErrorCode};

/// An error response. A 5xx carries a generic message; the cause is logged
/// under the correlation id instead, so nothing internal reaches a client.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    /// Its message is the one the client sees.
    error: Error,
}

impl ApiError {
    pub(crate) fn new(
        status: StatusCode,
        code: ErrorCode,
        message: impl Into<String>,
        correlation_id: &CorrelationId,
    ) -> Self {
        Self {
            status,
            error: Error::new(code, message).with_correlation_id(correlation_id.clone()),
        }
    }

    /// `error` as a response: InvalidInput is 400, NotFound 404, Conflict
    /// 409; Unavailable, Timeout and Cancelled are 503; anything else 500.
    pub(crate) fn from_error(error: &Error, correlation_id: &CorrelationId) -> Self {
        let code = error.code();
        let status = match code {
            ErrorCode::InvalidInput => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Unavailable | ErrorCode::Timeout | ErrorCode::Cancelled => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = match status {
            StatusCode::SERVICE_UNAVAILABLE => "the service is unavailable; try again",
            StatusCode::INTERNAL_SERVER_ERROR => {
                "the request failed; the server log has the cause under this correlation id"
            }
            _ => error.message(),
        };
        if status.is_server_error() {
            tracing::error!(
                correlation_id = correlation_id.as_str(),
                %error,
                "a request failed"
            );
        }
        Self::new(status, code, message, correlation_id)
    }

    pub(crate) fn invalid(message: impl Into<String>, correlation_id: &CorrelationId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidInput,
            message,
            correlation_id,
        )
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.error.to_info())).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
