//! Agents as configuration (blueprint section 14.3): the registry of specs,
//! the checks that fit a task to its agent, and the one loop type every
//! agent runs, `bounded_worker`. A specialist is a spec, not a new loop.

pub(crate) mod bounded_worker;
pub(crate) mod factory;
pub(crate) mod registry;

pub use registry::{AgentDefinition, AgentRegistry};
