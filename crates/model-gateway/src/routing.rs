//! Model profiles from `config/models/routing.yaml`, after the checks its
//! schema cannot express (ADR-0027).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use contracts::ids::Identifier;
use contracts::types::{
    AgentSpec, ModelHeaders, ModelPrice, ModelProfile, ModelRetryPolicy, ModelRouting,
};
use contracts::{Error, ErrorCode};
use reqwest::header::{HeaderName, HeaderValue};

use crate::env::Env;

/// Replaces `default_profile` for a run when set.
pub const PROFILE_OVERRIDE_ENV: &str = "SYNTH_MODEL_PROFILE";

/// Headers the gateway sets itself, or that carry credentials. A profile may
/// not set them: credentials come from environment variables only.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
];
const RESERVED_HEADER_PREFIX: &str = "anthropic-";

/// The validated routing configuration: profile names are unique, the default
/// profile exists, each priced model is priced once, and every header is
/// valid and not reserved.
#[derive(Clone, Debug)]
pub struct Routing {
    config: ModelRouting,
}

impl Routing {
    /// `InvalidInput` listing every problem found.
    pub fn new(config: ModelRouting) -> Result<Self, Error> {
        let mut problems = Vec::new();

        let mut names = BTreeSet::new();
        for profile in &config.profiles {
            let name = profile_name(profile);
            if !names.insert(name.as_str()) {
                problems.push(format!("profile `{}` is defined twice", name.as_str()));
            }
            problems.extend(header_problems(profile));
            let retry = profile_retry(profile);
            if retry.initial_backoff_ms > retry.max_backoff_ms {
                problems.push(format!(
                    "profile `{}`: retry.initial_backoff_ms exceeds retry.max_backoff_ms",
                    name.as_str()
                ));
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
            Ok(Self { config })
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

    pub fn profile(&self, name: &str) -> Option<&ModelProfile> {
        self.config
            .profiles
            .iter()
            .find(|p| profile_name(p).as_str() == name)
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
            None => self.profile(self.config.default_profile.as_str()).ok_or_else(|| {
                Error::new(ErrorCode::Internal, "the default profile was checked to exist")
            }),
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
        if !agent.tools.is_empty() && !profile_tool_calling(profile) {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "agent `{}` has tools, but its model profile `{}` has tool_calling: false; give the agent a profile with tool calling",
                    agent.id.as_str(),
                    profile_name(profile).as_str()
                ),
            ));
        }
        Ok(profile)
    }

    /// The checks of [`Routing::profile_for_agent`] against the configured
    /// default, without `SYNTH_MODEL_PROFILE`. For `cargo xtask validate-config`.
    pub fn check_agent(&self, agent: &AgentSpec) -> Result<(), Error> {
        let default = self
            .profile(self.config.default_profile.as_str())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Internal,
                    "the default profile was checked to exist",
                )
            })?;
        self.profile_for_agent(agent, default).map(|_| ())
    }

    fn profile_list(&self) -> String {
        join(
            self.config
                .profiles
                .iter()
                .map(|p| profile_name(p).as_str()),
        )
    }
}

fn join<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

fn header_problems(profile: &ModelProfile) -> Vec<String> {
    let name = profile_name(profile).as_str();
    let Some(headers) = profile_headers(profile) else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for (header, value) in headers.iter() {
        let lower = header.to_ascii_lowercase();
        if HeaderName::from_bytes(header.as_bytes()).is_err() {
            problems.push(format!(
                "profile `{name}`: `{header}` is not a valid header name"
            ));
        } else if RESERVED_HEADERS.contains(&lower.as_str())
            || lower.starts_with(RESERVED_HEADER_PREFIX)
        {
            problems.push(format!(
                "profile `{name}`: header `{header}` is set by the gateway or carries credentials, and may not be configured"
            ));
        }
        if HeaderValue::from_str(value.as_str()).is_err() {
            problems.push(format!(
                "profile `{name}`: header `{header}` has an invalid value"
            ));
        }
    }
    problems
}

// Fields every profile has, whatever its provider.

pub(crate) fn profile_name(profile: &ModelProfile) -> &Identifier {
    match profile {
        ModelProfile::Anthropic { name, .. } | ModelProfile::OpenaiCompatible { name, .. } => name,
    }
}

pub(crate) fn profile_model(profile: &ModelProfile) -> &str {
    match profile {
        ModelProfile::Anthropic { model, .. } | ModelProfile::OpenaiCompatible { model, .. } => {
            model.as_str()
        }
    }
}

pub(crate) fn profile_tool_calling(profile: &ModelProfile) -> bool {
    match profile {
        ModelProfile::Anthropic { tool_calling, .. }
        | ModelProfile::OpenaiCompatible { tool_calling, .. } => *tool_calling,
    }
}

pub(crate) fn profile_headers(profile: &ModelProfile) -> Option<&ModelHeaders> {
    match profile {
        ModelProfile::Anthropic { headers, .. }
        | ModelProfile::OpenaiCompatible { headers, .. } => headers.as_ref(),
    }
}

pub(crate) fn profile_retry(profile: &ModelProfile) -> &ModelRetryPolicy {
    match profile {
        ModelProfile::Anthropic { retry, .. } | ModelProfile::OpenaiCompatible { retry, .. } => {
            retry
        }
    }
}

pub(crate) fn profile_max_output_tokens(profile: &ModelProfile) -> u64 {
    match profile {
        ModelProfile::Anthropic {
            max_output_tokens, ..
        }
        | ModelProfile::OpenaiCompatible {
            max_output_tokens, ..
        } => max_output_tokens.get(),
    }
}

pub(crate) fn profile_timeout(profile: &ModelProfile) -> Duration {
    match profile {
        ModelProfile::Anthropic {
            request_timeout_seconds,
            ..
        }
        | ModelProfile::OpenaiCompatible {
            request_timeout_seconds,
            ..
        } => Duration::from_secs(request_timeout_seconds.get()),
    }
}

/// The profile's extra headers as name and value pairs, sorted by name.
pub(crate) fn header_pairs(profile: &ModelProfile) -> Vec<(String, String)> {
    let sorted: BTreeMap<&String, &str> = profile_headers(profile)
        .map(|h| h.iter().map(|(k, v)| (k, v.as_str())).collect())
        .unwrap_or_default();
    sorted
        .into_iter()
        .map(|(k, v)| (k.clone(), v.to_owned()))
        .collect()
}
