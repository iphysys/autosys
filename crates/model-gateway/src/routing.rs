//! Model profiles from `config/models/routing.yaml`, after the checks its
//! schema cannot express (ADR-0027, ADR-0028). Nothing here names a
//! provider: each profile's provider and settings are checked by the
//! registered provider.

use std::collections::BTreeSet;
use std::sync::Arc;

use contracts::types::{AgentSpec, ModelPrice, ModelProfile, ModelRouting};
use contracts::{Error, ErrorCode};

use crate::env::Env;
use crate::registry::ProviderRegistry;

/// Replaces `default_profile` for a run when set.
pub const PROFILE_OVERRIDE_ENV: &str = "SYNTH_MODEL_PROFILE";

/// The validated routing configuration: profile names are unique, the default
/// profile exists, each priced model is priced once, and every profile's
/// provider is registered and accepts its settings.
#[derive(Clone, Debug)]
pub struct Routing {
    config: ModelRouting,
    registry: Arc<ProviderRegistry>,
}

impl Routing {
    /// `InvalidInput` listing every problem found.
    pub fn new(config: ModelRouting, registry: Arc<ProviderRegistry>) -> Result<Self, Error> {
        let mut problems = Vec::new();

        let mut names = BTreeSet::new();
        for profile in &config.profiles {
            let name = profile.name.as_str();
            if !names.insert(name) {
                problems.push(format!("profile `{name}` is defined twice"));
            }
            if profile.retry.initial_backoff_ms > profile.retry.max_backoff_ms {
                problems.push(format!(
                    "profile `{name}`: retry.initial_backoff_ms exceeds retry.max_backoff_ms"
                ));
            }
            if let Err(e) = registry.check_profile(profile) {
                problems.push(format!("profile `{name}`: {}", e.message()));
            }
        }
        if !names.contains(config.default_profile.as_str()) {
            problems.push(format!(
                "default_profile `{}` is not a profile (profiles: {})",
                config.default_profile.as_str(),
                join(names.iter().copied())
            ));
        }

        let mut priced = BTreeSet::new();
        for price in &config.prices {
            if !priced.insert(price.model.as_str()) {
                problems.push(format!("model `{}` is priced twice", price.model.as_str()));
            }
        }

        if problems.is_empty() {
            Ok(Self { config, registry })
        } else {
            Err(Error::new(
                ErrorCode::InvalidInput,
                format!("model routing: {}", problems.join("; ")),
            ))
        }
    }

    pub fn config(&self) -> &ModelRouting {
        &self.config
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    pub fn profile(&self, name: &str) -> Option<&ModelProfile> {
        self.config
            .profiles
            .iter()
            .find(|p| p.name.as_str() == name)
    }

    pub fn price(&self, model: &str) -> Option<&ModelPrice> {
        self.config
            .prices
            .iter()
            .find(|p| p.model.as_str() == model)
    }

    /// The default profile, replaced by `SYNTH_MODEL_PROFILE` when that is set.
    pub fn resolve_default(&self, env: &dyn Env) -> Result<&ModelProfile, Error> {
        match env.var(PROFILE_OVERRIDE_ENV) {
            Some(name) => self.profile(&name).ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "{PROFILE_OVERRIDE_ENV} names `{name}`, which is not a profile in config/models/routing.yaml (profiles: {})",
                        self.profile_list()
                    ),
                )
            }),
            None => self.configured_default(),
        }
    }

    /// The profile `agent` uses: its own, or `default`. An agent with tools
    /// is refused a profile without tool calling.
    pub fn profile_for_agent<'a>(
        &'a self,
        agent: &AgentSpec,
        default: &'a ModelProfile,
    ) -> Result<&'a ModelProfile, Error> {
        let profile = match &agent.model_profile {
            Some(name) => self.profile(name.as_str()).ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "agent `{}`: model_profile `{}` is not a profile in config/models/routing.yaml (profiles: {})",
                        agent.id.as_str(),
                        name.as_str(),
                        self.profile_list()
                    ),
                )
            })?,
            None => default,
        };
        if !agent.tools.is_empty() && !profile.tool_calling {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "agent `{}` has tools, but its model profile `{}` has tool_calling: false; give the agent a profile with tool calling",
                    agent.id.as_str(),
                    profile.name.as_str()
                ),
            ));
        }
        Ok(profile)
    }

    /// The checks of [`Routing::profile_for_agent`] against the configured
    /// default, without `SYNTH_MODEL_PROFILE`. For `cargo xtask validate-config`.
    pub fn check_agent(&self, agent: &AgentSpec) -> Result<(), Error> {
        self.profile_for_agent(agent, self.configured_default()?)
            .map(|_| ())
    }

    fn configured_default(&self) -> Result<&ModelProfile, Error> {
        self.profile(self.config.default_profile.as_str())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Internal,
                    "the default profile was checked to exist",
                )
            })
    }

    fn profile_list(&self) -> String {
        join(self.config.profiles.iter().map(|p| p.name.as_str()))
    }
}

fn join<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}
