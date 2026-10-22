//! Test support for every trait in `synth-contracts`: in-memory stores, fakes,
//! builders for valid instances, and one contract-test suite per trait.
//!
//! The suites in [`contract`] define what every implementation must do. They
//! run here against the in-memory implementations, and later crates run them
//! against their own (for example the Postgres stores in PR 4).
//!
//! Only ever a dev-dependency (blueprint section 10.3; `cargo xtask arch-check`
//! enforces it).

// A test-support crate: a panic here is a test failure, which is what the
// suites and builders are for.
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod builders;
pub mod clock;
pub mod contract;
pub mod fakes;
pub mod stores;

pub use clock::ManualClock;
