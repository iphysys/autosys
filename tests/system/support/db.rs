//! A throwaway database per test, on the server at `DATABASE_URL`. The
//! worker connects to it by URL, as it would in production; it migrates it
//! itself.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

const TIMEOUT: Duration = Duration::from_secs(10);

static DATABASES: AtomicU64 = AtomicU64::new(0);

pub struct TestDb {
    server: PgPool,
    name: String,
    url: String,
}

impl TestDb {
    /// Creates an empty database. Panics with instructions when
    /// `DATABASE_URL` is unset: these tests never skip silently.
    pub async fn create() -> Self {
        let server_url = std::env::var("DATABASE_URL").expect(
            "DATABASE_URL must point at a Postgres server where tests may create databases \
             (see README, section Database)",
        );
        let options = PgConnectOptions::from_str(&server_url)
            .expect("DATABASE_URL is not a valid Postgres URL");
        let server = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(TIMEOUT)
            .connect_with(options)
            .await
            .expect("cannot connect to DATABASE_URL");
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        let name = format!(
            "autosys_system_{}_{started}_{}",
            std::process::id(),
            DATABASES.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&server)
            .await
            .expect("cannot create a test database");
        let url = with_database(&server_url, &name);
        Self { server, name, url }
    }

    /// The new database's URL: `DATABASE_URL` with its database replaced.
    /// It carries the same credentials, so it is never printed.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Drops the database. A test that panics before this leaves its
    /// `autosys_system_*` database behind; drop it by hand.
    pub async fn drop(self) {
        sqlx::query(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.server)
        .await
        .expect("cannot drop the test database");
    }
}

/// `url` with its path, the database name, replaced by `database`; any
/// query string is kept.
fn with_database(url: &str, database: &str) -> String {
    let authority = url.find("://").map_or(0, |i| i + 3);
    let (base, query) = match url[authority..].find('?') {
        Some(q) => url.split_at(authority + q),
        None => (url, ""),
    };
    let host_end = base[authority..]
        .find('/')
        .map_or(base.len(), |i| authority + i);
    format!("{}/{database}{query}", &base[..host_end])
}

#[test]
fn the_database_name_is_replaced_and_the_query_kept() {
    assert_eq!(
        with_database("postgres://u:p@h:5432/autosys?sslmode=disable", "x"),
        "postgres://u:p@h:5432/x?sslmode=disable"
    );
    assert_eq!(with_database("postgres://h", "x"), "postgres://h/x");
}
