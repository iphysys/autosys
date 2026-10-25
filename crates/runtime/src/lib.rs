//! The runtime: the orchestrator and its query loop (blueprint sections 3.1,
//! 5.2, 5.4 and 13.1, ADR-0030).
//!
//! - [`Orchestrator::run`] runs a job's task with the agent it names, in the
//!   `bounded_worker` loop: it builds each call's context, calls the model,
//!   runs the tool calls, and stops with one of the five stop reasons of
//!   blueprint section 5.4. It returns a [`RunStream`] of the run's events,
//!   then its [`RunOutcome`].
//! - [`AgentRegistry`] holds the agent specs from `config/agents/` and
//!   refuses one the runtime could not run. It also checks that a task fits
//!   its agent: the agent's tools, skills and limits are the ceiling.
//! - The capability router shows the model the task's tools, defined by
//!   their canonical schemas, and runs each call: `sdr_read` and
//!   `sdr_propose` in process, every other tool through the tool gateway.
//!   A proposal commits only through validation, the checks and the gates.
//! - Success is decided by the checks and gates of a final commit, never by
//!   the model. A run that waits for an approval returns a
//!   [`Continuation`], which [`Orchestrator::resume`] continues.
//!
//! Only `SdrManager` writes design state; the runtime reaches the SDR
//! through it alone.

mod agents;
mod budget;
mod cancellation;
mod capability_router;
mod error;
mod events;
mod orchestrator;
mod query_loop;
mod sdr_tools;
mod stop;
mod tool_schemas;

pub use agents::{AgentDefinition, AgentRegistry};
pub use error::RuntimeError;
pub use orchestrator::{Components, Orchestrator};
pub use query_loop::{Continuation, RunEvent, RunOutcome, RunStream};
