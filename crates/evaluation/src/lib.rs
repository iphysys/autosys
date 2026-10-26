//! Checks produce evidence; gates decide (blueprint section 15).
//!
//! The checks here are deterministic: schema validity, units (with a unit
//! registry by dimension), and numeric requirement thresholds (ADR-0021).
//! [`Evaluator`] runs them on a validated SDR transition, stores each finding
//! as evidence and records a `check_completed` event.
//!
//! [`benchmarks`] turns a benchmark problem into the request that starts a
//! run on it (ADR-0034), and [`expression`] evaluates its reference values
//! (ADR-0025).

pub mod baseline;
pub mod benchmarks;
pub mod checks;
mod evaluator;
pub mod expression;
pub mod outcomes;
pub mod units;

pub use evaluator::Evaluator;
