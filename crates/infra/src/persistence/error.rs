//! Contract errors for the Postgres stores. Database failures keep their
//! source; transient ones get a retryable code.

use contracts::{Error, ErrorCode};

/// A database failure during `op`.
pub(crate) fn db_error(op: &str, err: sqlx::Error) -> Error {
    let code = match &err {
        sqlx::Error::PoolTimedOut => ErrorCode::Timeout,
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => ErrorCode::Unavailable,
        sqlx::Error::Configuration(_) => ErrorCode::InvalidInput,
        sqlx::Error::Database(db) => sqlstate_code(db.code().as_deref()),
        _ => ErrorCode::Internal,
    };
    Error::new(code, format!("database error during {op}")).with_source(err)
}

/// Maps a Postgres SQLSTATE to an error code.
fn sqlstate_code(state: Option<&str>) -> ErrorCode {
    match state {
        // query_canceled: the server's statement_timeout fired.
        Some("57014") => ErrorCode::Timeout,
        // serialization_failure and deadlock_detected: safe to retry.
        Some("40001" | "40P01") => ErrorCode::Unavailable,
        // admin_shutdown, crash_shutdown, cannot_connect_now.
        Some("57P01" | "57P02" | "57P03") => ErrorCode::Unavailable,
        // Class 08: connection exceptions.
        Some(s) if s.starts_with("08") => ErrorCode::Unavailable,
        _ => ErrorCode::Internal,
    }
}

/// Whether `err` is a violated unique or primary-key constraint, meaning
/// another writer stored the same key first.
pub(crate) fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.is_unique_violation())
}

pub(crate) fn not_found(what: &str, id: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("{what} {id} does not exist"))
}

pub(crate) fn conflict(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Conflict, message)
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

/// Stored data the store itself wrote no longer has the expected shape.
pub(crate) fn corrupt(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal, message)
}

/// A count or version as a Postgres `bigint`.
pub(crate) fn to_bigint(value: u64, what: &str) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| invalid(format!("{what} {value} is too large to store")))
}

/// A `bigint` column that the schema keeps non-negative.
pub(crate) fn from_bigint(value: i64, what: &str) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| corrupt(format!("stored {what} {value} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstates_map_to_retryable_codes_only_when_transient() {
        assert_eq!(sqlstate_code(Some("57014")), ErrorCode::Timeout);
        for transient in ["40001", "40P01", "57P01", "08006", "08001"] {
            assert_eq!(
                sqlstate_code(Some(transient)),
                ErrorCode::Unavailable,
                "{transient}"
            );
        }
        for permanent in ["23505", "23503", "42P01", "22P02"] {
            assert_eq!(
                sqlstate_code(Some(permanent)),
                ErrorCode::Internal,
                "{permanent}"
            );
        }
        assert_eq!(sqlstate_code(None), ErrorCode::Internal);
    }

    #[test]
    fn pool_and_connection_failures_are_retryable() {
        assert!(db_error("op", sqlx::Error::PoolTimedOut).retryable());
        assert!(db_error("op", sqlx::Error::PoolClosed).retryable());
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(db_error("op", sqlx::Error::Io(io)).retryable());
        assert!(!db_error("op", sqlx::Error::RowNotFound).retryable());
    }

    #[test]
    fn bigint_conversions_reject_out_of_range_values() {
        assert_eq!(to_bigint(7, "version").ok(), Some(7));
        assert_eq!(
            to_bigint(u64::MAX, "version").map_err(|e| e.code()),
            Err(ErrorCode::InvalidInput)
        );
        assert_eq!(
            from_bigint(-1, "attempt").map_err(|e| e.code()),
            Err(ErrorCode::Internal)
        );
    }
}
