//! Adapters that implement the `synth-contracts` traits on real
//! infrastructure. Only `bootstrap` constructs them (blueprint section 13.1).
//!
//! Backends are Cargo features: `postgres` (default) adds the PostgreSQL
//! stores. Configuration loading is always available.

pub mod config;
#[cfg(feature = "postgres")]
pub mod persistence;
