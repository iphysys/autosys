//! Throwaway databases for the integration tests. Each test creates its own
//! database on the server at `DATABASE_URL`, migrates it and drops it at the
//! end, so tests run in parallel without seeing each other's data.

#![allow(
    clippy::expect_used,
    dead_code,
    reason = "test support: a broken test database must fail the test at once, and not every test binary uses every helper"
)]

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use contracts::Clock;
use infra::persistence::{
    Database, PgEventLog, PgJobStore, PgSdrRepository, PgStepStore, PoolSettings,
};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};
use testkit::contract::FencedStores;

/// Limit on each store operation, and on connecting.
pub const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: u32 = 8;

static DATABASES: AtomicU64 = AtomicU64::new(0);

pub struct TestDb {
    server: PgPool,
    options: PgConnectOptions,
    name: String,
    pool: PgPool,
}

impl TestDb {
    /// Creates and migrates a new database. Panics with instructions when
    /// `DATABASE_URL` is unset: these tests never skip silently.
    pub async fn create() -> Self {
        let db = Self::unmigrated().await;
        db.database()
            .migrate()
            .await
            .expect("cannot migrate the test database");
        db
    }

    /// Creates a database migrated up to `version` only, for a test of the
    /// migrations after it.
    pub async fn create_at(version: i64) -> Self {
        let db = Self::unmigrated().await;
        db.migrate_to(version)
            .await
            .expect("cannot migrate the test database");
        db
    }

    /// Applies the migrations up to `version`.
    pub async fn migrate_to(&self, version: i64) -> Result<(), sqlx::migrate::MigrateError> {
        infra::persistence::MIGRATOR
            .run_to(version, &self.pool)
            .await
    }

    /// The pool, for statements the stores do not make.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn unmigrated() -> Self {
        let url = std::env::var("DATABASE_URL").expect(
            "DATABASE_URL must point at a Postgres server where tests may create databases \
             (see README, section Database)",
        );
        let server_options =
            PgConnectOptions::from_str(&url).expect("DATABASE_URL is not a valid Postgres URL");
        let server = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(TIMEOUT)
            .connect_with(server_options.clone())
            .await
            .expect("cannot connect to DATABASE_URL");
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        let name = format!(
            "autosys_test_{}_{started}_{}",
            std::process::id(),
            DATABASES.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&server)
            .await
            .expect("cannot create a test database");
        let options = server_options.database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(TIMEOUT)
            .connect_with(options.clone())
            .await
            .expect("cannot connect to the test database");
        Self {
            server,
            options,
            name,
            pool,
        }
    }

    /// Stores built from this share one pool.
    pub fn database(&self) -> Database {
        Database::new(self.pool.clone(), TIMEOUT)
    }

    /// A new pool connected the way `bootstrap` connects, as a second worker
    /// process would have.
    pub async fn second_process(&self, max_connections: u32) -> Database {
        let settings = PoolSettings {
            max_connections,
            acquire_timeout: TIMEOUT,
        };
        Database::connect_with(self.options.clone(), &settings, TIMEOUT)
            .await
            .expect("cannot connect a second pool")
    }

    /// Deletes every row, so the next store starts empty.
    pub async fn empty(&self) {
        self.sql(
            "TRUNCATE jobs, events, event_sequences, sdr_versions, evidence, artifacts, step_records".into(),
        )
        .await;
    }

    /// Replaces the database clock with the session setting
    /// `autosys.test_now`, which [`Self::following`] keeps in step with a
    /// test clock. Only this throwaway database is affected.
    pub async fn use_session_clock(&self) {
        self.sql(
            "CREATE OR REPLACE FUNCTION autosys_now() RETURNS timestamptz LANGUAGE sql STABLE \
             AS $$ SELECT current_setting('autosys.test_now')::timestamptz $$"
                .into(),
        )
        .await;
    }

    /// Stops the database clock at `at`.
    pub async fn stop_clock_at(&self, at: DateTime<Utc>) {
        self.sql(format!(
            "CREATE OR REPLACE FUNCTION autosys_now() RETURNS timestamptz LANGUAGE sql STABLE \
             AS $$ SELECT '{}'::timestamptz $$",
            at.to_rfc3339()
        ))
        .await;
    }

    /// A pool whose connections set `autosys.test_now` from `clock` each
    /// time they are handed out, so with [`Self::use_session_clock`] the
    /// database reads the time that `clock` shows at the start of each store
    /// operation.
    pub fn following(&self, clock: Arc<dyn Clock>) -> Database {
        let on_connect = Arc::clone(&clock);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(TIMEOUT)
            .after_connect(move |conn, _meta| {
                let now = on_connect.now();
                Box::pin(async move { set_session_clock(conn, now).await })
            })
            .before_acquire(move |conn, _meta| {
                let now = clock.now();
                Box::pin(async move { set_session_clock(conn, now).await.map(|()| true) })
            })
            .connect_lazy_with(self.options.clone());
        Database::new(pool, TIMEOUT)
    }

    /// The job store, event log, step store and SDR repository on one pool
    /// that [`Self::following`] `clock`, for the fencing suite. Needs
    /// [`Self::use_session_clock`].
    pub async fn fenced_stores(&self, clock: Arc<dyn Clock>) -> FencedStores {
        let database = self.following(clock);
        FencedStores {
            jobs: Arc::new(PgJobStore::new(database.clone())),
            events: Arc::new(
                PgEventLog::connect(database.clone())
                    .await
                    .expect("cannot start the event listener"),
            ),
            steps: Arc::new(PgStepStore::new(database.clone())),
            sdr: Arc::new(PgSdrRepository::new(database)),
        }
    }

    /// The database clock, read directly.
    pub async fn now(&self) -> DateTime<Utc> {
        sqlx::query_scalar("SELECT now()")
            .fetch_one(&self.pool)
            .await
            .expect("cannot read the database clock")
    }

    /// Drops the database. A test that panics before this leaves its
    /// `autosys_test_*` database behind; drop it by hand.
    pub async fn drop(self) {
        self.pool.close().await;
        sqlx::query(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.server)
        .await
        .expect("cannot drop the test database");
    }

    async fn sql(&self, statement: String) {
        sqlx::query(AssertSqlSafe(statement))
            .execute(&self.pool)
            .await
            .expect("test setup statement failed");
    }
}

async fn set_session_clock(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('autosys.test_now', $1, false)")
        .bind(now.to_rfc3339())
        .execute(conn)
        .await
        .map(|_| ())
}
