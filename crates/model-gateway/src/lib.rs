//! The model gateway (blueprint section 14.4, ADR-0027): the harness side of
//! every model call.
//!
//! - [`Routing`]: model profiles from `config/models/routing.yaml`, checked.
//! - [`ModelGateway`]: resolves each agent's profile at startup, refusing a
//!   profile without tool calling for an agent with tools; builds a provider
//!   for every profile in use, failing on any missing environment variable;
//!   and runs calls with token and cost accounting.
//! - [`providers`]: one adapter per provider API behind
//!   `contracts::ModelProvider`. Provider wire types stay inside each adapter.
//!
//! Credentials come from environment variables only and are never logged;
//! neither are prompts.

pub mod env;
mod gateway;
mod http;
mod pricing;
pub mod providers;
mod retry;
mod routing;

pub use env::{Env, ProcessEnv, Secret};
pub use gateway::{ModelCall, ModelGateway};
pub use pricing::cost_usd;
pub use providers::{ModelUsage, ProviderAdapter, ProviderCall};
pub use routing::{PROFILE_OVERRIDE_ENV, Routing};
