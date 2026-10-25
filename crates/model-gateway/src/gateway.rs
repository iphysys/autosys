//! Profile resolution at startup, and model calls with token and cost
//! accounting. Nothing here names a provider: adapters are built by the
//! provider each profile names (ADR-0028).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use contracts::ids::{AgentId, Identifier};
use contracts::types::{AgentSpec, ModelProfile};
use contracts::{Error, ErrorCode, ExecutionContext, ModelRequest, ModelResponse};

use crate::env::Env;
use crate::pricing::cost_usd;
use crate::providers::{ModelUsage, ProviderAdapter};
use crate::routing::Routing;

/// One completed model call.
#[derive(Clone, Debug)]
pub struct ModelCall {
    pub profile: Identifier,
    pub response: ModelResponse,
    /// USD at the configured prices; `None` when a model that ran has no
    /// price. Attempts a refusal fallback replaced are included.
    pub cost_usd: Option<f64>,
    pub usage_by_model: Vec<ModelUsage>,
    /// HTTP attempts, retries included.
    pub attempts: u32,
    pub duration: Duration,
}

/// The model gateway. Built once at startup by [`ModelGateway::start`]; every
/// configuration problem fails there, not at the first call.
pub struct ModelGateway {
    routing: Routing,
    default_profile: Identifier,
    agent_profiles: BTreeMap<String, Identifier>,
    providers: BTreeMap<String, Arc<dyn ProviderAdapter>>,
}

impl ModelGateway {
    /// Resolves the default profile (`SYNTH_MODEL_PROFILE` overrides it) and
    /// the profile of each agent in `agents`, refusing a profile without tool
    /// calling for an agent with tools. Then builds a provider for every
    /// profile in use, reading its environment variables; a missing one fails
    /// with an error naming it. Profiles not in use need no variables.
    pub fn start(routing: Routing, agents: &[AgentSpec], env: &dyn Env) -> Result<Self, Error> {
        let plan = Plan::resolve(&routing, agents, env)?;
        let mut providers: BTreeMap<String, Arc<dyn ProviderAdapter>> = BTreeMap::new();
        let mut problems = Vec::new();
        for name in &plan.in_use {
            let Some(profile) = routing.profile(name) else {
                continue;
            };
            match build_provider(&routing, profile, env) {
                Ok(provider) => {
                    providers.insert(name.clone(), provider);
                }
                Err(e) => problems.push(e.message().to_owned()),
            }
        }
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("model gateway: {}", problems.join("; ")),
            ));
        }
        Ok(Self::assemble(routing, plan, providers))
    }

    /// As [`ModelGateway::start`], with providers the caller built, keyed by
    /// profile name: for example adapters pointed at a test server. Every
    /// profile in use needs one.
    pub fn with_providers(
        routing: Routing,
        agents: &[AgentSpec],
        env: &dyn Env,
        providers: BTreeMap<String, Arc<dyn ProviderAdapter>>,
    ) -> Result<Self, Error> {
        let plan = Plan::resolve(&routing, agents, env)?;
        let missing: Vec<&str> = plan
            .in_use
            .iter()
            .filter(|name| !providers.contains_key(*name))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "model gateway: no provider for profiles in use: {}",
                    missing.join(", ")
                ),
            ));
        }
        Ok(Self::assemble(routing, plan, providers))
    }

    fn assemble(
        routing: Routing,
        plan: Plan,
        providers: BTreeMap<String, Arc<dyn ProviderAdapter>>,
    ) -> Self {
        Self {
            routing,
            default_profile: plan.default_profile,
            agent_profiles: plan.agent_profiles,
            providers,
        }
    }

    /// The routing the gateway was started with, read-only: callers read a
    /// profile's limits here, such as its `max_output_tokens`.
    pub fn routing(&self) -> &Routing {
        &self.routing
    }

    /// The profile of agents that name none, after `SYNTH_MODEL_PROFILE`.
    pub fn default_profile(&self) -> &Identifier {
        &self.default_profile
    }

    /// The profile `agent` was given at startup. `NotFound` for an agent that
    /// was not passed to [`ModelGateway::start`], since its profile was never
    /// checked.
    pub fn agent_profile(&self, agent: &AgentId) -> Result<&Identifier, Error> {
        self.agent_profiles.get(agent.as_str()).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!(
                    "agent `{}` was not checked at startup, so it has no model profile",
                    agent.as_str()
                ),
            )
        })
    }

    /// One model call with `profile`. The gateway sets `request.model_id` to
    /// the profile's model, replacing any value; `max_output_tokens` may not
    /// exceed the profile's. Logs one event with the profile, model, tokens,
    /// cost and attempts; never the prompt.
    pub async fn complete(
        &self,
        profile: &Identifier,
        mut request: ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelCall, Error> {
        let (Some(provider), Some(config)) = (
            self.providers.get(profile.as_str()),
            self.routing.profile(profile.as_str()),
        ) else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!(
                    "model profile `{}` is not in use (profiles in use: {})",
                    profile.as_str(),
                    self.providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        };
        let limit = config.max_output_tokens.get();
        if u64::from(request.max_output_tokens) > limit {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "max_output_tokens {} exceeds the {limit} of model profile `{}`",
                    request.max_output_tokens,
                    profile.as_str()
                ),
            ));
        }
        config.model.as_str().clone_into(&mut request.model_id);

        let started = Instant::now();
        let result = provider.call(&request, ctx).await;
        let duration = started.elapsed();
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(call) => {
                let cost = cost_usd(&self.routing, &call.usage_by_model);
                tracing::info!(
                    profile = profile.as_str(),
                    provider = provider.provider_id().as_str(),
                    model = call.response.model_id.as_str(),
                    stop_reason = ?call.response.stop_reason,
                    input_tokens = call.response.usage.input_tokens,
                    output_tokens = call.response.usage.output_tokens,
                    cost_usd = ?cost,
                    attempts = call.attempts,
                    duration_ms,
                    request_id = ?call.request_id,
                    "model call completed"
                );
                Ok(ModelCall {
                    profile: profile.clone(),
                    response: call.response,
                    cost_usd: cost,
                    usage_by_model: call.usage_by_model,
                    attempts: call.attempts,
                    duration,
                })
            }
            Err(e) => {
                tracing::warn!(
                    profile = profile.as_str(),
                    provider = provider.provider_id().as_str(),
                    code = %e.code(),
                    duration_ms,
                    error = e.message(),
                    "model call failed"
                );
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests;

/// Which profile everything uses.
struct Plan {
    default_profile: Identifier,
    agent_profiles: BTreeMap<String, Identifier>,
    in_use: BTreeSet<String>,
}

impl Plan {
    fn resolve(routing: &Routing, agents: &[AgentSpec], env: &dyn Env) -> Result<Self, Error> {
        let default = routing.resolve_default(env)?;
        let default_profile = default.name.clone();
        let mut in_use = BTreeSet::from([default_profile.as_str().to_owned()]);
        let mut agent_profiles = BTreeMap::new();
        let mut problems = Vec::new();
        for agent in agents {
            match routing.profile_for_agent(agent, default) {
                Ok(profile) => {
                    let name = &profile.name;
                    in_use.insert(name.as_str().to_owned());
                    agent_profiles.insert(agent.id.as_str().to_owned(), name.clone());
                }
                Err(e) => problems.push(e.message().to_owned()),
            }
        }
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("model gateway: {}", problems.join("; ")),
            ));
        }
        Ok(Self {
            default_profile,
            agent_profiles,
            in_use,
        })
    }
}

/// The adapter for `profile`, built by its registered provider, which reads
/// the profile's environment variables.
fn build_provider(
    routing: &Routing,
    profile: &ModelProfile,
    env: &dyn Env,
) -> Result<Arc<dyn ProviderAdapter>, Error> {
    routing
        .registry()
        .factory(profile.provider.as_str())
        .and_then(|factory| factory.build(profile, env, routing.environment()))
        .map_err(|e| {
            Error::new(
                e.code(),
                format!("model profile `{}`: {}", profile.name.as_str(), e.message()),
            )
            .with_source(e)
        })
}
