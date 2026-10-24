//! Test support shared by the gateway's and the providers' tests: a loopback
//! replay server, and a fake provider so tests of the gateway itself depend
//! on no real provider.

pub(crate) mod replay;

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::{ModelProfile, ModelRetryPolicy, ModelRouting};
use contracts::{
    ContentBlock, Error, ExecutionContext, Message, ModelProvider, ModelRequest, ModelResponse,
    Role, ToolSpec,
};
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::fakes::{ScriptedModelProvider, text_response};

use crate::env::Env;
use crate::providers::{EnvReader, ModelUsage, ProviderAdapter, ProviderCall, default_registry};
use crate::registry::{ProviderFactory, ProviderRegistry};
use crate::routing::Routing;

pub(crate) fn ctx() -> ExecutionContext {
    ExecutionContext::new(id("corr-model-gateway"))
}

/// Short waits, so retry tests stay fast.
pub(crate) fn retry(max_attempts: u64) -> ModelRetryPolicy {
    ModelRetryPolicy {
        max_attempts: NonZeroU64::new(max_attempts).expect("non-zero"),
        initial_backoff_ms: 1,
        max_backoff_ms: 10,
    }
}

/// A request for `model` with one user message and the `create_plate` tool.
pub(crate) fn request(model: &str, text: &str) -> ModelRequest {
    ModelRequest {
        model_id: model.to_owned(),
        system: "You design mechanical parts.".to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }],
        tools: vec![ToolSpec {
            name: id("create_plate"),
            description: "Builds a rectangular plate.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": { "length_mm": { "type": "number" } },
                "required": ["length_mm"]
            }),
        }],
        max_output_tokens: 1024,
    }
}

pub(crate) fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
    vars.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// A path relative to the crate directory.
pub(crate) fn crate_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

pub(crate) fn test_ca_path() -> String {
    crate_path("src/test_support/fixtures/test-ca.pem")
        .display()
        .to_string()
}

/// `config/models/routing.yaml` as committed, checked against the providers
/// the harness ships with.
pub(crate) fn committed_routing() -> Routing {
    let text = std::fs::read_to_string(crate_path("../../config/models/routing.yaml"))
        .expect("routing.yaml");
    let config: ModelRouting = serde_saphyr::from_str(&text).expect("routing.yaml parses");
    Routing::new(config, Arc::new(default_registry().expect("registry")))
        .expect("routing.yaml is consistent")
}

/// The variable the `fake` provider's profiles need.
pub(crate) const FAKE_KEY_ENV: &str = "FAKE_PROVIDER_KEY";
/// The variable the `other_fake` provider's profiles need.
pub(crate) const OTHER_FAKE_KEY_ENV: &str = "OTHER_FAKE_PROVIDER_KEY";

/// A provider without settings whose adapter answers from a script. Its
/// profiles need `key_env` set, so startup errors can be tested.
pub(crate) struct FakeFactory {
    pub(crate) name: &'static str,
    pub(crate) key_env: &'static str,
}

impl ProviderFactory for FakeFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn settings_schema(&self) -> Option<&'static str> {
        None
    }

    fn check(&self, _profile: &ModelProfile) -> Result<(), Error> {
        Ok(())
    }

    fn build(
        &self,
        _profile: &ModelProfile,
        env: &dyn Env,
    ) -> Result<Arc<dyn ProviderAdapter>, Error> {
        let mut vars = EnvReader::new(env);
        vars.required(&parse(json!(self.key_env)));
        vars.finish()?;
        Ok(Arc::new(FakeAdapter::answering(text_response("Done."))))
    }
}

/// A registry with two fake providers, `fake` and `other_fake`.
pub(crate) fn fake_registry() -> Arc<ProviderRegistry> {
    Arc::new(
        ProviderRegistry::new(vec![
            Arc::new(FakeFactory {
                name: "fake",
                key_env: FAKE_KEY_ENV,
            }),
            Arc::new(FakeFactory {
                name: "other_fake",
                key_env: OTHER_FAKE_KEY_ENV,
            }),
        ])
        .expect("registry"),
    )
}

/// A profile of the `fake` provider; `tool_calling` as given.
pub(crate) fn fake_profile(name: &str, tool_calling: bool) -> Value {
    profile_of("fake", name, tool_calling)
}

/// A profile of the given fake provider.
pub(crate) fn profile_of(provider: &str, name: &str, tool_calling: bool) -> Value {
    json!({
        "name": name, "provider": provider, "model": format!("{name}-model"),
        "tool_calling": tool_calling, "max_output_tokens": 1000,
        "request_timeout_seconds": 10,
        "retry": { "max_attempts": 1, "initial_backoff_ms": 0, "max_backoff_ms": 0 },
        "settings": {}
    })
}

/// A routing over the fake provider.
pub(crate) fn fake_routing(default_profile: &str, profiles: Vec<Value>, prices: Value) -> Routing {
    let config: ModelRouting = parse(json!({
        "schema_version": "0.2",
        "default_profile": default_profile,
        "profiles": profiles,
        "prices": prices
    }));
    Routing::new(config, fake_registry()).expect("consistent routing")
}

/// A scripted model behind the `ProviderAdapter` trait.
pub(crate) struct FakeAdapter(ScriptedModelProvider);

impl FakeAdapter {
    pub(crate) fn answering(response: ModelResponse) -> Self {
        Self(ScriptedModelProvider::new([Ok(response)]))
    }
}

#[async_trait]
impl ModelProvider for FakeAdapter {
    fn provider_id(&self) -> &Identifier {
        self.0.provider_id()
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        self.0.complete(request, ctx).await
    }
}

#[async_trait]
impl ProviderAdapter for FakeAdapter {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        let response = self.0.complete(request, ctx).await?;
        Ok(ProviderCall {
            usage_by_model: vec![ModelUsage {
                model: response.model_id.clone(),
                usage: response.usage.clone(),
            }],
            response,
            attempts: 1,
            request_id: None,
        })
    }
}
