//! A live run on Pliny, run by hand (ADR-0029, ADR-0030, ADR-0036): does
//! the provider accept a context rebuilt in the middle of a tool-use turn?
//!
//! The loop rebuilds the whole context for every call, so the second call
//! of a turn starts with a first user message that differs from the one the
//! model answered (the job state and the run history moved on), followed by
//! the model's reply and the tool result. If the provider refuses that, the
//! runtime has to pin the turn's snapshot and job view instead (ADR-0029's
//! fallback).
//!
//! Ignored by default: it needs `PLINY_API_KEY` and `PLINY_BASE_URL`.
//! `SYNTH_MODEL_PROFILE` picks the profile; the default is `pliny`.
//!
//! ```sh
//! cargo test -p synth-runtime --test live -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::{EventPayload, ModelRouting};
use contracts::{
    ContentBlock, Error, ExecutionContext, ModelProvider, ModelRequest, ModelResponse,
};
use model_gateway::{Env, ProcessEnv, ProviderAdapter, ProviderCall, Routing};
use serde_json::json;
use support::*;

/// The real adapter, with every response kept for the assertions.
struct Recording {
    inner: Arc<dyn ProviderAdapter>,
    responses: Mutex<Vec<ModelResponse>>,
}

#[async_trait]
impl ModelProvider for Recording {
    fn provider_id(&self) -> &Identifier {
        self.inner.provider_id()
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        self.call(request, ctx).await.map(|call| call.response)
    }
}

#[async_trait]
impl ProviderAdapter for Recording {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        let call = self.inner.call(request, ctx).await?;
        self.responses.lock().unwrap().push(call.response.clone());
        Ok(call)
    }
}

#[tokio::test]
#[ignore = "calls Pliny; needs PLINY_API_KEY and PLINY_BASE_URL"]
async fn live_rebuilt_context_mid_turn() {
    let profile_name = ProcessEnv
        .var(model_gateway::PROFILE_OVERRIDE_ENV)
        .unwrap_or_else(|| "pliny".to_owned());
    let config: ModelRouting = committed("models/routing.yaml");
    let routing = Routing::new(
        config,
        Arc::new(model_gateway::providers::default_registry().expect("providers")),
        contracts::Environment::Test,
    )
    .expect("routing");
    let profile = routing
        .profile(&profile_name)
        .unwrap_or_else(|| panic!("no profile {profile_name}"))
        .clone();
    let real = routing
        .registry()
        .factory(profile.provider.as_str())
        .and_then(|factory| factory.build(&profile, &ProcessEnv, contracts::Environment::Test))
        .expect("the profile's variables are set");
    let recording = Arc::new(Recording {
        inner: real,
        responses: Mutex::new(Vec::new()),
    });
    let world = World::new(Setup::new(Vec::new()).provider(&profile_name, recording.clone())).await;

    let mut task = task_json(&["R-mass"], budget(200_000, 4, 4, 600, 1));
    task["tools"] = json!(["create_plate"]);
    task["goal"] = json!(
        "Build one plate with the create_plate tool: length 110 mm, width 70 mm, \
         thickness 6 mm, holes 6.5 mm in diameter, hole inset 10 mm. Then reply in one \
         sentence with the part_id it returned, and stop."
    );
    let ran = world.run(job_for(task)).await;

    let responses = recording.responses.lock().unwrap().clone();
    let first = responses.first().expect("at least one model call");
    let called = first.content.iter().any(
        |b| matches!(b, ContentBlock::ToolCall { name, .. } if name.as_str() == "create_plate"),
    );
    eprintln!(
        "live: profile {profile_name}, {} model calls, first reply: tool call {called}",
        responses.len()
    );
    assert!(
        called,
        "inconclusive: the first reply called no tool, so no turn was open; run it again"
    );

    // The call that matters: the second, inside the open turn.
    if let Err(error) = &ran.outcome {
        panic!(
            "the provider refused a request after {} model calls: {error}; \
             apply ADR-0029's fallback and pin the turn's snapshot",
            responses.len()
        );
    }
    assert!(responses.len() >= 2, "the turn's second call was made");
    let hashes: Vec<String> = ran
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ContextAssembled { context_hash, .. } => {
                Some(context_hash.as_str().to_owned())
            }
            _ => None,
        })
        .collect();
    assert!(hashes.len() >= 2);
    assert_ne!(
        hashes[0], hashes[1],
        "the second context was rebuilt, not reused"
    );
    assert_eq!(world.create_plate.calls(), 1);
    eprintln!("live: the run stopped with {:?}", ran.outcome().stop_reason);
}
