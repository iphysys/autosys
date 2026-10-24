//! The providers the gateway can build adapters for (ADR-0028). Each
//! provider registers one factory; nothing else in the gateway names a
//! provider, so adding or removing one touches only its own module, its
//! settings schema and its line in `providers::default_registry`.

use std::fmt;
use std::sync::Arc;

use contracts::types::ModelProfile;
use contracts::{Error, ErrorCode, SchemaSet};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::env::Env;
use crate::providers::ProviderAdapter;

/// One provider: its name in `routing.yaml`, the schema of its `settings`,
/// and how it builds an adapter for a profile.
pub trait ProviderFactory: Send + Sync {
    /// The `provider` value of the profiles it serves.
    fn name(&self) -> &'static str;

    /// The schema of its profiles' `settings`, relative to `schemas/`. `None`
    /// for a provider without settings, whose `settings` must be empty.
    fn settings_schema(&self) -> Option<&'static str>;

    /// Checks of a profile that its settings schema cannot express, such as
    /// reserved headers. Runs after the schema check and needs no
    /// environment, so `cargo xtask validate-config` runs it too.
    fn check(&self, profile: &ModelProfile) -> Result<(), Error>;

    /// The adapter for a profile in use, with its credentials and endpoint
    /// read from `env`. A missing variable fails, naming it.
    fn build(
        &self,
        profile: &ModelProfile,
        env: &dyn Env,
    ) -> Result<Arc<dyn ProviderAdapter>, Error>;
}

/// The registered providers, in registration order. Built explicitly at
/// startup (blueprint section 13.1); a name registered twice fails.
pub struct ProviderRegistry {
    factories: Vec<Arc<dyn ProviderFactory>>,
    schemas: SchemaSet,
}

impl ProviderRegistry {
    pub fn new(factories: Vec<Arc<dyn ProviderFactory>>) -> Result<Self, Error> {
        let mut names: Vec<&str> = Vec::new();
        for factory in &factories {
            if names.contains(&factory.name()) {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("model provider `{}` is registered twice", factory.name()),
                ));
            }
            names.push(factory.name());
        }
        let schema_paths: Vec<&str> = factories
            .iter()
            .filter_map(|f| f.settings_schema())
            .collect();
        Ok(Self {
            schemas: SchemaSet::compile(&schema_paths)?,
            factories,
        })
    }

    /// The registered provider names, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.factories.iter().map(|f| f.name()).collect()
    }

    /// The factory registered as `provider`. The error lists the registered
    /// providers.
    pub fn factory(&self, provider: &str) -> Result<&dyn ProviderFactory, Error> {
        self.factories
            .iter()
            .find(|f| f.name() == provider)
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "model provider `{provider}` is not registered (registered: {})",
                        self.names().join(", ")
                    ),
                )
            })
    }

    /// Checks that the profile's provider is registered, that its `settings`
    /// satisfy the provider's schema, and the provider's own checks. The
    /// caller names the profile in the error.
    pub fn check_profile(&self, profile: &ModelProfile) -> Result<(), Error> {
        let factory = self.factory(profile.provider.as_str())?;
        let problems = match factory.settings_schema() {
            Some(schema) => self
                .schemas
                .errors(schema, &Value::Object(profile.settings.clone()))?,
            None if profile.settings.is_empty() => Vec::new(),
            None => vec![format!("provider `{}` takes no settings", factory.name())],
        };
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("settings: {}", problems.join("; ")),
            ));
        }
        factory.check(profile)
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// The profile's `settings` as the provider's generated settings type. For
/// use in a factory, after [`ProviderRegistry::check_profile`] passed.
pub fn settings<T: DeserializeOwned>(profile: &ModelProfile) -> Result<T, Error> {
    serde_json::from_value(Value::Object(profile.settings.clone())).map_err(|e| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("profile `{}`: settings: {e}", profile.name.as_str()),
        )
    })
}
