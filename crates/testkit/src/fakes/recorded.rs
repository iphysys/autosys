use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::{Error, ErrorCode, ExecutionContext, ModelProvider, ModelRequest, ModelResponse};
use serde::Deserialize;

/// Replays a recording of provider-neutral model exchanges, so tests that make
/// model calls never reach a live model. Each request must equal the recorded
/// one; a different request, or one past the end of the recording, is an
/// `Internal` error, so a test whose prompts drift fails loudly.
///
/// A recording is JSON:
///
/// ```json
/// { "provider": "recorded", "exchanges": [ { "request": {..}, "response": {..} } ] }
/// ```
///
/// `request` and `response` use the serde form of [`ModelRequest`] and
/// [`ModelResponse`].
pub struct RecordedModelProvider {
    id: Identifier,
    exchanges: Vec<Exchange>,
    next: Mutex<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    provider: Identifier,
    exchanges: Vec<Exchange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    request: ModelRequest,
    response: ModelResponse,
}

impl RecordedModelProvider {
    /// Parses a recording. `InvalidInput` if it is not one.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        let recording: Recording = serde_json::from_str(text).map_err(|e| {
            Error::new(
                ErrorCode::InvalidInput,
                format!("not a model recording: {e}"),
            )
        })?;
        Ok(Self {
            id: recording.provider,
            exchanges: recording.exchanges,
            next: Mutex::new(0),
        })
    }

    /// Reads and parses the recording at `path`.
    pub fn from_file(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                ErrorCode::NotFound,
                format!("{}: cannot read: {e}", path.display()),
            )
        })?;
        Self::from_json(&text)
            .map_err(|e| Error::new(e.code(), format!("{}: {}", path.display(), e.message())))
    }

    /// Exchanges not yet replayed.
    pub fn remaining(&self) -> usize {
        self.exchanges.len() - *self.next.lock().unwrap()
    }
}

#[async_trait]
impl ModelProvider for RecordedModelProvider {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        ctx.ensure_not_cancelled()?;
        let mut next = self.next.lock().unwrap();
        let call = *next + 1;
        let Some(exchange) = self.exchanges.get(*next) else {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("model recording exhausted at call {call}"),
            ));
        };
        if let Some(field) = first_difference(&exchange.request, request) {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("call {call}: request differs from the recording in `{field}`"),
            ));
        }
        *next += 1;
        Ok(exchange.response.clone())
    }
}

/// The first field in which the requests differ, if any.
fn first_difference(recorded: &ModelRequest, actual: &ModelRequest) -> Option<&'static str> {
    if recorded.model_id != actual.model_id {
        Some("model_id")
    } else if recorded.system != actual.system {
        Some("system")
    } else if recorded.messages != actual.messages {
        Some("messages")
    } else if recorded.tools != actual.tools {
        Some("tools")
    } else if recorded.max_output_tokens != actual.max_output_tokens {
        Some("max_output_tokens")
    } else {
        None
    }
}
