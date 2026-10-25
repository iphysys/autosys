//! System tests: workers built by bootstrap, on a throwaway Postgres
//! database at `DATABASE_URL`, with a scripted model and fake tools
//! (blueprint section 16.1). Each test creates and drops its own database.

mod startup;
mod support;
