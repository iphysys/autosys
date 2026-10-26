//! Model calls as replay records (ADR-0035).
//!
//! A completed call becomes a [`ModelCallRecord`] and its blobs: the system
//! prompt, each block of each message, the tool definitions and each block
//! of the reply, each stored once as a JSON value keyed by the sha256 of its
//! canonical JSON ([`canonical_sha256`]). Blocks are the gateway's
//! `ContentBlock`s, whose JSON the schema's `TurnBlock` repeats, so opaque
//! provider blocks come back unchanged. [`rebuild`] turns a record and its
//! blobs back into the request and the reply.

use std::collections::BTreeMap;

use schema_types::{
    ModelCallRecord, ModelCallRecordSchemaVersion, ReplayMessage, ReplayMessageRole,
    ReplayResponse, ReplayStopReason, Sha256,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::canonical::canonical_sha256;
use crate::error::{Error, ErrorCode};
use crate::ids::{Identifier, RunId};
use crate::model::{
    ContentBlock, Message, ModelRequest, ModelResponse, ModelStopReason, Role, ToolSpec,
};
use crate::storage::ReplayStore;

/// One stored value of a replay record, keyed by the sha256 of its canonical
/// JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayBlob {
    pub sha256: Sha256,
    pub content: Value,
}

impl ReplayBlob {
    pub fn new(content: Value) -> Result<Self, Error> {
        Ok(Self {
            sha256: canonical_sha256(&content)?,
            content,
        })
    }
}

/// A record with the blobs it names, each once.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayCall {
    pub record: ModelCallRecord,
    pub blobs: Vec<ReplayBlob>,
}

/// What identifies a call, beside what it sent and received.
#[derive(Clone, Debug)]
pub struct CallKey<'a> {
    pub run_id: &'a RunId,
    /// The seq of the call's `context_assembled` event.
    pub context_seq: u64,
    /// The `context_hash` that event logged.
    pub context_hash: &'a Sha256,
    pub model_profile: &'a Identifier,
}

/// The record of `request` answered by `response`. The model id is the
/// reply's: the gateway sets it, so the request's may be empty.
pub fn record_call(
    key: &CallKey<'_>,
    request: &ModelRequest,
    response: &ModelResponse,
) -> Result<ReplayCall, Error> {
    let mut blobs = Blobs::default();
    let system = blobs.add(&request.system)?;
    let messages = request
        .messages
        .iter()
        .map(|message| {
            Ok(ReplayMessage {
                role: match message.role {
                    Role::User => ReplayMessageRole::User,
                    Role::Assistant => ReplayMessageRole::Assistant,
                },
                blocks: blobs.add_all(&message.content)?,
            })
        })
        .collect::<Result<_, Error>>()?;
    let tools = blobs.add(&request.tools)?;
    let response_blocks = blobs.add_all(&response.content)?;
    let record = ModelCallRecord {
        schema_version: ModelCallRecordSchemaVersion::X01,
        run_id: key.run_id.clone(),
        context_seq: std::num::NonZeroU64::new(key.context_seq)
            .ok_or_else(|| invalid("a call's context_seq is at least 1"))?,
        context_hash: key.context_hash.clone(),
        model_profile: key.model_profile.clone(),
        model_id: response
            .model_id
            .parse()
            .map_err(|_| invalid("a reply's model id is empty"))?,
        max_output_tokens: std::num::NonZeroU64::new(u64::from(request.max_output_tokens))
            .ok_or_else(|| invalid("a call's output limit is at least 1"))?,
        system,
        messages,
        tools,
        response: ReplayResponse {
            blocks: response_blocks,
            stop_reason: convert(&response.stop_reason)?,
            usage: response.usage.clone(),
        },
    };
    Ok(ReplayCall {
        record,
        blobs: blobs.into_vec(),
    })
}

/// The request and the reply `record` stands for, from `blobs` by hash.
/// NotFound if a blob it names is missing; InvalidInput if a blob is not
/// what its place in the record needs.
pub fn rebuild(
    record: &ModelCallRecord,
    blobs: &BTreeMap<String, Value>,
) -> Result<(ModelRequest, ModelResponse), Error> {
    let get = |hash: &Sha256| -> Result<&Value, Error> {
        blobs.get(hash.as_str()).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("replay blob {} is not stored", hash.as_str()),
            )
        })
    };
    let blocks = |hashes: &[Sha256]| -> Result<Vec<ContentBlock>, Error> {
        hashes.iter().map(|h| from_blob(get(h)?, "block")).collect()
    };
    let request = ModelRequest {
        model_id: record.model_id.to_string(),
        system: from_blob(get(&record.system)?, "system prompt")?,
        messages: record
            .messages
            .iter()
            .map(|m| {
                Ok(Message {
                    role: match m.role {
                        ReplayMessageRole::User => Role::User,
                        ReplayMessageRole::Assistant => Role::Assistant,
                    },
                    content: blocks(&m.blocks)?,
                })
            })
            .collect::<Result<_, Error>>()?,
        tools: from_blob::<Vec<ToolSpec>>(get(&record.tools)?, "tool list")?,
        max_output_tokens: u32::try_from(record.max_output_tokens.get())
            .map_err(|_| invalid("a recorded output limit does not fit u32"))?,
    };
    let response = ModelResponse {
        model_id: record.model_id.to_string(),
        content: blocks(&record.response.blocks)?,
        stop_reason: convert::<ReplayStopReason, ModelStopReason>(&record.response.stop_reason)?,
        usage: record.response.usage.clone(),
    };
    Ok((request, response))
}

/// The blobs `record` names, read from `store`, by hash: what [`rebuild`]
/// takes.
pub async fn fetch(
    store: &dyn ReplayStore,
    record: &ModelCallRecord,
) -> Result<BTreeMap<String, Value>, Error> {
    let mut blobs = BTreeMap::new();
    for hash in blob_hashes(record) {
        if !blobs.contains_key(hash.as_str()) {
            let blob = store.blob(hash).await?;
            blobs.insert(blob.sha256.as_str().to_owned(), blob.content);
        }
    }
    Ok(blobs)
}

/// Every blob hash `record` names, in order, with repeats.
pub fn blob_hashes(record: &ModelCallRecord) -> Vec<&Sha256> {
    let mut hashes = vec![&record.system, &record.tools];
    for message in &record.messages {
        hashes.extend(&message.blocks);
    }
    hashes.extend(&record.response.blocks);
    hashes
}

/// What a store checks before storing `blobs` for `record`: each blob's key
/// is the hash of its content, and every hash the record names is among
/// `blobs` or `stored`. InvalidInput otherwise.
pub fn verify(
    record: &ModelCallRecord,
    blobs: &[ReplayBlob],
    stored: impl Fn(&Sha256) -> bool,
) -> Result<(), Error> {
    for blob in blobs {
        if canonical_sha256(&blob.content)? != blob.sha256 {
            return Err(invalid(format!(
                "replay blob {} does not hash to its key",
                blob.sha256.as_str()
            )));
        }
    }
    for hash in blob_hashes(record) {
        if !blobs.iter().any(|b| &b.sha256 == hash) && !stored(hash) {
            return Err(invalid(format!(
                "the record names blob {}, which is neither given nor stored",
                hash.as_str()
            )));
        }
    }
    Ok(())
}

/// The blobs of one record, each once, in first-use order.
#[derive(Default)]
struct Blobs {
    blobs: Vec<ReplayBlob>,
}

impl Blobs {
    fn add<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<Sha256, Error> {
        let content = serde_json::to_value(value)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("a replay blob: {e}")))?;
        let blob = ReplayBlob::new(content)?;
        let hash = blob.sha256.clone();
        if !self.blobs.iter().any(|b| b.sha256 == hash) {
            self.blobs.push(blob);
        }
        Ok(hash)
    }

    fn add_all<T: Serialize>(&mut self, values: &[T]) -> Result<Vec<Sha256>, Error> {
        values.iter().map(|v| self.add(v)).collect()
    }

    fn into_vec(self) -> Vec<ReplayBlob> {
        self.blobs
    }
}

fn from_blob<T: DeserializeOwned>(value: &Value, what: &str) -> Result<T, Error> {
    serde_json::from_value(value.clone())
        .map_err(|e| invalid(format!("a replay blob is not a {what}: {e}")))
}

/// Between two types with the same JSON.
fn convert<A: Serialize, B: DeserializeOwned>(value: &A) -> Result<B, Error> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|e| Error::new(ErrorCode::Internal, format!("a replay field: {e}")))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "tests")]

    use schema_types::{ReplayToolSpec, TokenUsage, TurnBlock};
    use serde_json::{Map, json};

    use super::*;

    fn request() -> ModelRequest {
        let sections = vec![
            ContentBlock::Text {
                text: "<context-section name=\"task_contract\">…".into(),
            },
            ContentBlock::Text {
                text: "<context-section name=\"design_state\">…".into(),
            },
        ];
        let mut arguments = Map::new();
        arguments.insert("length".into(), json!(110.0));
        let mut data = Map::new();
        data.insert("signature".into(), json!("abc"));
        data.insert("thinking".into(), json!("…"));
        ModelRequest {
            model_id: String::new(),
            system: "You are the orchestrator.".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: sections,
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Opaque {
                            provider: "anthropic".parse().expect("id"),
                            data,
                        },
                        ContentBlock::ToolCall {
                            id: "call-1".into(),
                            name: "create_plate".parse().expect("name"),
                            arguments,
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        call_id: "call-1".into(),
                        content: "{\"ok\":true}".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "create_plate".parse().expect("name"),
                description: "Creates a plate.".into(),
                input_schema: json!({ "type": "object" }),
            }],
            max_output_tokens: 16000,
        }
    }

    fn response() -> ModelResponse {
        ModelResponse {
            model_id: "claude-haiku-5-5".into(),
            content: vec![ContentBlock::Text {
                // The same text as a block the request sent: stored once.
                text: "<context-section name=\"task_contract\">…".into(),
            }],
            stop_reason: ModelStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 5000,
                output_tokens: 120,
            },
        }
    }

    fn recorded() -> ReplayCall {
        let run_id: RunId = "run-1".parse().expect("id");
        let hash = canonical_sha256(&json!("context")).expect("hash");
        let profile: Identifier = "haiku".parse().expect("id");
        record_call(
            &CallKey {
                run_id: &run_id,
                context_seq: 4,
                context_hash: &hash,
                model_profile: &profile,
            },
            &request(),
            &response(),
        )
        .expect("records")
    }

    fn by_hash(blobs: &[ReplayBlob]) -> BTreeMap<String, Value> {
        blobs
            .iter()
            .map(|b| (b.sha256.as_str().to_owned(), b.content.clone()))
            .collect()
    }

    #[test]
    fn a_call_round_trips_through_its_record() {
        let call = recorded();
        let (request, response) = rebuild(&call.record, &by_hash(&call.blobs)).expect("rebuilds");
        assert_eq!(
            request,
            ModelRequest {
                model_id: "claude-haiku-5-5".into(),
                ..self::request()
            }
        );
        assert_eq!(response, self::response());
        assert_eq!(call.record.context_seq.get(), 4);
    }

    #[test]
    fn a_block_sent_twice_is_stored_once() {
        let call = recorded();
        let named = blob_hashes(&call.record);
        // system, tools, 2 + 2 + 1 request blocks, 1 reply block.
        assert_eq!(named.len(), 8);
        assert_eq!(call.blobs.len(), 7, "the repeated section is one blob");
        verify(&call.record, &call.blobs, |_| false).expect("complete");
    }

    #[test]
    fn blobs_follow_the_schema_types() {
        let call = recorded();
        for message in &call.record.messages {
            for hash in &message.blocks {
                let blob = call
                    .blobs
                    .iter()
                    .find(|b| &b.sha256 == hash)
                    .expect("given");
                let block: TurnBlock =
                    serde_json::from_value(blob.content.clone()).expect("a TurnBlock");
                assert_eq!(serde_json::to_value(block).expect("json"), blob.content);
            }
        }
        let tools = call
            .blobs
            .iter()
            .find(|b| b.sha256 == call.record.tools)
            .expect("given");
        let specs: Vec<ReplayToolSpec> =
            serde_json::from_value(tools.content.clone()).expect("tool specs");
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn a_wrong_or_missing_blob_is_refused() {
        let call = recorded();
        let mut forged = call.blobs.clone();
        forged[0].content = json!("something else");
        let error = verify(&call.record, &forged, |_| false).expect_err("forged");
        assert_eq!(error.code(), ErrorCode::InvalidInput);

        let missing: Vec<ReplayBlob> = call.blobs[1..].to_vec();
        let error = verify(&call.record, &missing, |_| false).expect_err("missing");
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        verify(&call.record, &missing, |h| *h == call.blobs[0].sha256).expect("already stored");

        let error = rebuild(&call.record, &by_hash(&missing)).expect_err("not stored");
        assert_eq!(error.code(), ErrorCode::NotFound);
    }
}
