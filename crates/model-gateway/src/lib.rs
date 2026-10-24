//! The model gateway (blueprint section 14.4, ADR-0027, ADR-0028): the
//! harness side of every model call.
//!
//! - [`Routing`]: model profiles from `config/models/routing.yaml`, checked
//!   against the registered providers.
//! - [`ProviderRegistry`]: the providers the gateway can build adapters for;
//!   [`providers::default_registry`] lists the ones the harness ships with.
//! - [`ModelGateway`]: resolves each agent's profile at startup, refusing a
//!   profile without tool calling for an agent with tools; builds an adapter
//!   for every profile in use, failing on any missing environment variable;
//!   and runs calls with token and cost accounting.
//! - [`providers`]: one module per provider behind `contracts::ModelProvider`.
//!   Provider wire types stay inside each module.
//!
//! Credentials come from environment variables only and are never logged;
//! neither are prompts.

pub mod env;
mod gateway;
mod http;
mod pricing;
pub mod providers;
mod registry;
mod retry;
mod routing;
#[cfg(test)]
mod test_support;

pub use env::{Env, ProcessEnv, Secret};
pub use gateway::{ModelCall, ModelGateway};
pub use pricing::cost_usd;
pub use providers::{ModelUsage, ProviderAdapter, ProviderCall};
pub use registry::{ProviderFactory, ProviderRegistry, settings};
pub use routing::{PROFILE_OVERRIDE_ENV, Routing};
