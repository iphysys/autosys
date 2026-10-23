//! PostgreSQL persistence (ADR-0016) for jobs, events, SDR versions, evidence,
//! step records and artifact metadata. Artifact content is stored as files on
//! a mounted volume.
//!
//! Every timestamp a store sets, lease expiry included, comes from the
//! database clock through the `autosys_now()` SQL function, so workers on
//! different hosts agree on time whatever their own clocks say (ADR-0023). No
//! store here takes a [`contracts::Clock`].
//!
//! Queries are checked at compile time against the database at `DATABASE_URL`
//! or, with `SQLX_OFFLINE=true`, against the query data committed in `.sqlx/`.

mod artifacts;
mod error;
mod events;
mod evidence;
mod jobs;
mod sdr;
mod steps;

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use contracts::{Error, ErrorCode};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

pub use artifacts::PgArtifactStore;
pub use events::PgEventLog;
pub use evidence::PgEvidenceStore;
pub use jobs::PgJobStore;
pub use sdr::PgSdrRepository;
pub use steps::PgStepStore;

use error::db_error;

/// The schema migrations in `crates/infra/migrations/`, embedded at build time.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Connection pool limits.
#[derive(Clone, Debug, PartialEq)]
pub struct PoolSettings {
    pub max_connections: u32,
    /// How long an operation waits for a free connection.
    pub acquire_timeout: Duration,
}

/// A connection pool and the time limit on each store operation. Cloning is
/// cheap, and every store built from a clone shares the pool.
#[derive(Clone)]
pub struct Database {
    pool: PgPool,
    timeout: Duration,
}

impl Database {
    /// Wraps an existing pool. `timeout` bounds every store operation,
    /// including the wait for a connection.
    pub fn new(pool: PgPool, timeout: Duration) -> Self {
        Self { pool, timeout }
    }

    /// Connects to the database at `url`. The URL may carry credentials, so it
    /// never appears in errors or logs.
    pub async fn connect(
        url: &str,
        settings: &PoolSettings,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let options = PgConnectOptions::from_str(url).map_err(|e| {
            Error::new(ErrorCode::InvalidInput, "the database URL is not valid").with_source(e)
        })?;
        Self::connect_with(options, settings, timeout).await
    }

    /// Connects with parsed options. The server also cancels any statement
    /// that runs longer than `timeout`, so an abandoned query does not keep
    /// running after its caller gave up.
    pub async fn connect_with(
        options: PgConnectOptions,
        settings: &PoolSettings,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let statement_timeout = format!("{}ms", timeout.as_millis());
        let options = options.options([("statement_timeout", statement_timeout.as_str())]);
        let connecting = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .acquire_timeout(settings.acquire_timeout)
            .connect_with(options);
        let pool = tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| timed_out("connect", timeout))?
            .map_err(|e| db_error("connect", e))?;
        Ok(Self::new(pool, timeout))
    }

    /// Applies any migration not yet applied. Bounded by the operation
    /// timeout, like every other call.
    pub async fn migrate(&self) -> Result<(), Error> {
        self.bounded("migrate", async {
            MIGRATOR.run(&self.pool).await.map_err(|e| {
                Error::new(ErrorCode::Internal, "database migration failed").with_source(e)
            })
        })
        .await
    }

    /// Runs one store operation under the time limit. A future dropped at the
    /// limit rolls back any transaction it had open.
    async fn bounded<T>(
        &self,
        op: &str,
        operation: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        tokio::time::timeout(self.timeout, operation)
            .await
            .unwrap_or_else(|_| Err(timed_out(op, self.timeout)))
    }
}

fn timed_out(op: &str, limit: Duration) -> Error {
    Error::new(
        ErrorCode::Timeout,
        format!("{op} did not finish within {limit:?}"),
    )
}
