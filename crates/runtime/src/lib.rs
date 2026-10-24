//! The runtime: the orchestrator and its query loop (blueprint sections 3.1,
//! 5.2, 5.4 and 13.1, ADR-0030).
//!
//! - [`AgentRegistry`] holds the agent specs from `config/agents/` and
//!   refuses one the runtime could not run. It also checks that a task fits
//!   its agent: the agent's tools, skills and limits are the ceiling.
//!
//! Only `SdrManager` writes design state; the runtime reaches the SDR
//! through it alone.

mod agents;
mod error;
mod sdr_tools;

pub use agents::{AgentDefinition, AgentRegistry};
pub use error::RuntimeError;
