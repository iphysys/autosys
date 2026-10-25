//! System tests: workers built by bootstrap, on a throwaway Postgres
//! database at `DATABASE_URL`, with a scripted model and fake tools
//! (blueprint section 16.1). Each test creates and drops its own database.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod api;
mod crash_points;
mod report;
mod scenarios;
mod startup;
mod support;
mod worker_process;
