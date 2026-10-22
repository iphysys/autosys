use std::future::Future;

use chrono::{TimeDelta, TimeZone, Utc};
use contracts::ids::ArtifactId;
use contracts::{ArtifactStore, ErrorCode};

use super::assert_code;
use crate::builders::{artifact, id};

/// The `ArtifactStore` contract. `make` builds an empty store.
pub async fn artifact_store<S, F, Fut>(make: F)
where
    S: ArtifactStore,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    put_then_read(make().await).await;
    content_must_match_metadata(make().await).await;
    identical_put_is_idempotent(make().await).await;
    different_artifact_conflicts(make().await).await;
    list_for_run_is_ordered_and_scoped(make().await).await;
}

async fn put_then_read<S: ArtifactStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let content = b"ISO-10303-21; example".to_vec();
    let meta = artifact("art-1", "run-1", at, &content);
    store
        .put(&meta, &content)
        .await
        .expect("put_then_read: put");
    assert_eq!(
        store.metadata(&meta.id).await.expect("metadata"),
        meta,
        "put_then_read: metadata"
    );
    assert_eq!(
        store.content(&meta.id).await.expect("content"),
        content,
        "put_then_read: content"
    );
    let missing: ArtifactId = id("art-missing");
    assert_code(
        store.metadata(&missing).await,
        ErrorCode::NotFound,
        "put_then_read: missing metadata",
    );
    assert_code(
        store.content(&missing).await,
        ErrorCode::NotFound,
        "put_then_read: missing content",
    );
}

async fn content_must_match_metadata<S: ArtifactStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let meta = artifact("art-1", "run-1", at, b"abcd");
    assert_code(
        store.put(&meta, b"abc").await,
        ErrorCode::InvalidInput,
        "content_must_match_metadata: size",
    );
    assert_code(
        store.put(&meta, b"abce").await,
        ErrorCode::InvalidInput,
        "content_must_match_metadata: sha256",
    );
    assert_code(
        store.metadata(&meta.id).await,
        ErrorCode::NotFound,
        "content_must_match_metadata: a rejected artifact was stored",
    );
}

async fn identical_put_is_idempotent<S: ArtifactStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let meta = artifact("art-1", "run-1", at, b"abcd");
    store
        .put(&meta, b"abcd")
        .await
        .expect("idempotent: first put");
    store
        .put(&meta, b"abcd")
        .await
        .expect("idempotent: identical put was rejected");
    let listed = store.list_for_run(&meta.run_id).await.expect("list");
    assert_eq!(listed.len(), 1, "idempotent: stored twice");
}

async fn different_artifact_conflicts<S: ArtifactStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let original = artifact("art-1", "run-1", at, b"abcd");
    store
        .put(&original, b"abcd")
        .await
        .expect("conflict: first put");
    let replacement = artifact("art-1", "run-1", at, b"wxyz");
    assert_code(
        store.put(&replacement, b"wxyz").await,
        ErrorCode::Conflict,
        "different_artifact_conflicts",
    );
    assert_eq!(
        store.content(&original.id).await.expect("content"),
        b"abcd",
        "different_artifact_conflicts: content was overwritten"
    );
}

async fn list_for_run_is_ordered_and_scoped<S: ArtifactStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let later = artifact("art-a", "run-1", at + TimeDelta::seconds(5), b"1");
    let same_time_b = artifact("art-c", "run-1", at, b"2");
    let same_time_a = artifact("art-b", "run-1", at, b"3");
    let other_run = artifact("art-d", "run-2", at, b"4");
    for (meta, content) in [
        (&later, b"1"),
        (&same_time_b, b"2"),
        (&same_time_a, b"3"),
        (&other_run, b"4"),
    ] {
        store.put(meta, content).await.expect("list: put");
    }
    let listed = store.list_for_run(&id("run-1")).await.expect("list");
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["art-b", "art-c", "art-a"],
        "list_for_run: order or scope"
    );
}
