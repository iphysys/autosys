use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use contracts::{ToolOutput, ToolRequest};
use serde_json::Value;

use super::tool::{FakeTool, HookFuture};

#[derive(Default)]
struct Effects {
    stored: BTreeMap<String, Value>,
    calls: BTreeMap<String, usize>,
}

/// The durable side of a tool with an effect, shared by every instance of
/// the tool, as a worker started after a crash shares a real tool's store:
/// one effect per idempotency key (ADR-0026, ADR-0031). A call stores its
/// effect under its key unless one is there, and answers the stored one.
/// Cloning shares the store.
#[derive(Clone, Default)]
pub struct KeyedEffects {
    effects: Arc<Mutex<Effects>>,
}

impl KeyedEffects {
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool `name` over this store. A call computes `effect` from its
    /// request, stores it under the request's idempotency key unless an
    /// effect is there, then runs `after_put` (which may wait, for a crash
    /// between the effect and its result, or fail the call), and answers
    /// the stored effect. A plain [`FakeTool`]: its hook is the pause point.
    pub fn tool(
        &self,
        name: &str,
        effect: impl Fn(&ToolRequest) -> Value + Send + Sync + 'static,
        after_put: impl Fn(usize, &ToolRequest) -> HookFuture + Send + Sync + 'static,
    ) -> FakeTool {
        let store = self.clone();
        let answers = self.clone();
        FakeTool::new(name, move |request| {
            let key = request.idempotency_key.as_str();
            let stored = answers.effects.lock().unwrap().stored.get(key).cloned();
            Ok(ToolOutput {
                structured: stored.unwrap_or(Value::Null),
            })
        })
        .before_answer(move |index, request| {
            {
                let mut effects = store.effects.lock().unwrap();
                let key = request.idempotency_key.as_str().to_owned();
                *effects.calls.entry(key.clone()).or_default() += 1;
                effects.stored.entry(key).or_insert_with(|| effect(request));
            }
            after_put(index, request)
        })
    }

    /// How many keys hold an effect.
    pub fn effects(&self) -> usize {
        self.effects.lock().unwrap().stored.len()
    }

    /// The effect stored under `key`.
    pub fn effect(&self, key: &str) -> Option<Value> {
        self.effects.lock().unwrap().stored.get(key).cloned()
    }

    /// How many calls reached the store with `key`, across instances.
    pub fn calls(&self, key: &str) -> usize {
        self.effects
            .lock()
            .unwrap()
            .calls
            .get(key)
            .copied()
            .unwrap_or(0)
    }
}

/// An `after_put` that goes straight on.
pub fn go_on(_index: usize, _request: &ToolRequest) -> HookFuture {
    Box::pin(async { Ok(()) })
}
