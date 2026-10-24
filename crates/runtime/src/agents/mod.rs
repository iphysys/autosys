//! Agents as configuration (blueprint section 14.3): the registry of specs,
//! and the checks that fit a task to its agent. A specialist is a spec, not
//! a new loop; `bounded_worker` is the one loop type.

mod factory;
mod registry;

pub use registry::{AgentDefinition, AgentRegistry};
