//! The runtime: the orchestrator and its query loop (blueprint sections 3.1,
//! 5.2, 5.4 and 13.1, ADR-0030).
//!
//! - [`AgentRegistry`] holds the agent specs from `config/agents/` and
//!   refuses one the runtime could not run. It also checks that a task fits
//!   its agent: the agent's tools, skills and limits are the ceiling.
//! - The capability router shows the model the task's tools, defined by
//!   their canonical schemas, and runs each call: `sdr_read` and
//!   `sdr_propose` in process, every other tool through the tool gateway.
//!   A proposal commits only through validation, the checks and the gates.
//!
//! Only `SdrManager` writes design state; the runtime reaches the SDR
//! through it alone.

mod agents;
// The query loop that calls these lands in the next commit of PR 11.
#[allow(dead_code, reason = "called by the query loop, added next")]
mod cancellation;
#[allow(dead_code, reason = "called by the query loop, added next")]
mod capability_router;
mod error;
#[allow(dead_code, reason = "called by the query loop, added next")]
mod events;
#[allow(dead_code, reason = "called by the query loop, added next")]
mod sdr_tools;
#[allow(dead_code, reason = "called by the query loop, added next")]
mod tool_schemas;

pub use agents::{AgentDefinition, AgentRegistry};
pub use error::RuntimeError;
