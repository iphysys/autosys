//! Checks produce evidence; gates decide (blueprint section 15).
//!
//! The checks here are deterministic: schema validity, units (with a unit
//! registry by dimension), and numeric requirement thresholds (ADR-0021).
//! [`Evaluator`] runs them on a validated SDR transition, stores each finding
//! as evidence and records a `check_completed` event.

pub mod checks;
mod evaluator;
pub mod units;

pub use evaluator::Evaluator;
