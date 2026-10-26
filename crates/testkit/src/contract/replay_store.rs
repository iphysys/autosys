use std::future::Future;

use contracts::ids::{Identifier, RunId};
use contracts::replay::{CallKey, ReplayBlob, record_call};
use contracts::types::{Sha256, TokenUsage};
use contracts::{
    ContentBlock, ErrorCode, Message, ModelRequest, ModelResponse, ModelStopReason, ReplayCall,
    ReplayStore, Role, canonical_sha256,
};
use serde_json::json;

use super::assert_code;
use crate::builders::id;

/// The `ReplayStore` contract. `make` builds an empty store.
pub async fn replay_store<S, F, Fut>(make: F)
where
    S: ReplayStore,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    put_then_read(make().await).await;
    identical_put_is_idempotent(make().await).await;
    a_different_record_under_a_key_conflicts(make().await).await;
    blobs_are_checked(make().await).await;
    calls_share_blobs(make().await).await;
    calls_are_ordered_and_scoped(make().await).await;
}

/// A call of `run` at `seq` whose context holds `section`, with `reply`.
fn call(run: &str, seq: u64, section: &str, reply: &str) -> ReplayCall {
    let run_id: RunId = id(run);
    let profile: Identifier = id("haiku");
    let hash: Sha256 = canonical_sha256(&json!([run, seq, section])).expect("a hash");
    let request = ModelRequest {
        model_id: String::new(),
        system: "You are the orchestrator.".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "<context-section name=\"task_contract\">plate".into(),
                },
                ContentBlock::Text {
                    text: section.into(),
                },
            ],
        }],
        tools: Vec::new(),
        max_output_tokens: 1000,
    };
    let response = ModelResponse {
        model_id: "model-1".into(),
        content: vec![ContentBlock::Text { text: reply.into() }],
        stop_reason: ModelStopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
        },
    };
    record_call(
        &CallKey {
            run_id: &run_id,
            context_seq: seq,
            context_hash: &hash,
            model_profile: &profile,
        },
        &request,
        &response,
    )
    .expect("a call record")
}

async fn put_then_read<S: ReplayStore>(store: S) {
    let recorded = call("run-1", 3, "design v1", "done");
    store
        .put_call(&recorded.record, &recorded.blobs)
        .await
        .expect("put_then_read: put");
    let stored = store
        .call(&id("run-1"), 3)
        .await
        .expect("put_then_read: call");
    assert_eq!(
        stored, recorded.record,
        "put_then_read: stored record differs"
    );
    for blob in &recorded.blobs {
        let read = store.blob(&blob.sha256).await.expect("put_then_read: blob");
        assert_eq!(&read, blob, "put_then_read: stored blob differs");
    }
    assert_code(
        store.call(&id("run-1"), 4).await,
        ErrorCode::NotFound,
        "put_then_read: missing call",
    );
    let missing: Sha256 = canonical_sha256(&json!("never stored")).expect("a hash");
    assert_code(
        store.blob(&missing).await,
        ErrorCode::NotFound,
        "put_then_read: missing blob",
    );
}

async fn identical_put_is_idempotent<S: ReplayStore>(store: S) {
    let recorded = call("run-1", 3, "design v1", "done");
    store
        .put_call(&recorded.record, &recorded.blobs)
        .await
        .expect("idempotent: first put");
    store
        .put_call(&recorded.record, &recorded.blobs)
        .await
        .expect("idempotent: identical put was rejected");
    let calls = store.calls_for_run(&id("run-1")).await.expect("list");
    assert_eq!(calls.len(), 1, "idempotent: stored twice");
}

async fn a_different_record_under_a_key_conflicts<S: ReplayStore>(store: S) {
    let original = call("run-1", 3, "design v1", "done");
    store
        .put_call(&original.record, &original.blobs)
        .await
        .expect("conflict: first put");
    let other = call("run-1", 3, "design v1", "something else");
    assert_code(
        store.put_call(&other.record, &other.blobs).await,
        ErrorCode::Conflict,
        "a_different_record_under_a_key_conflicts",
    );
    let stored = store.call(&id("run-1"), 3).await.expect("conflict: call");
    assert_eq!(
        stored, original.record,
        "conflict: the record was overwritten"
    );
}

async fn blobs_are_checked<S: ReplayStore>(store: S) {
    let recorded = call("run-1", 3, "design v1", "done");
    let mut forged = recorded.blobs.clone();
    forged[0] = ReplayBlob {
        sha256: forged[0].sha256.clone(),
        content: json!("not what the key hashes"),
    };
    assert_code(
        store.put_call(&recorded.record, &forged).await,
        ErrorCode::InvalidInput,
        "blobs_are_checked: forged blob",
    );
    assert_code(
        store.put_call(&recorded.record, &recorded.blobs[1..]).await,
        ErrorCode::InvalidInput,
        "blobs_are_checked: a named blob is missing",
    );
    assert_code(
        store.call(&id("run-1"), 3).await,
        ErrorCode::NotFound,
        "blobs_are_checked: a refused record was stored",
    );
}

async fn calls_share_blobs<S: ReplayStore>(store: S) {
    let first = call("run-1", 3, "design v1", "next");
    store
        .put_call(&first.record, &first.blobs)
        .await
        .expect("share: first put");
    // The second call repeats the system prompt, the tools and the task
    // contract; only its new blobs need be given.
    let second = call("run-1", 7, "design v2", "done");
    let new: Vec<ReplayBlob> = second
        .blobs
        .iter()
        .filter(|b| !first.blobs.contains(b))
        .cloned()
        .collect();
    assert!(
        new.len() < second.blobs.len(),
        "share: the calls repeat blobs"
    );
    store
        .put_call(&second.record, &new)
        .await
        .expect("share: a call naming stored blobs");
    let shared = &first.blobs[0];
    assert_eq!(
        &store.blob(&shared.sha256).await.expect("share: blob"),
        shared
    );
}

async fn calls_are_ordered_and_scoped<S: ReplayStore>(store: S) {
    for (run, seq) in [("run-1", 12), ("run-1", 3), ("run-2", 5), ("run-1", 7)] {
        let recorded = call(run, seq, "design", "done");
        store
            .put_call(&recorded.record, &recorded.blobs)
            .await
            .expect("order: put");
    }
    let seqs: Vec<u64> = store
        .calls_for_run(&id("run-1"))
        .await
        .expect("order: list")
        .iter()
        .map(|c| c.context_seq.get())
        .collect();
    assert_eq!(seqs, [3, 7, 12], "calls_for_run: order or scope");
}
