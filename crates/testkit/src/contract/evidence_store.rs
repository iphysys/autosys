use std::future::Future;

use chrono::{TimeDelta, TimeZone, Utc};
use contracts::ids::{EvidenceId, RunId};
use contracts::{ErrorCode, EvidenceStore};

use super::assert_code;
use crate::builders::{evidence, id};

/// The `EvidenceStore` contract. `make` builds an empty store.
pub async fn evidence_store<S, F, Fut>(make: F)
where
    S: EvidenceStore,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    put_then_get(make().await).await;
    identical_put_is_idempotent(make().await).await;
    different_content_conflicts(make().await).await;
    list_for_run_is_ordered_and_scoped(make().await).await;
}

async fn put_then_get<S: EvidenceStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let record = evidence("ev-1", "run-1", at, "pass");
    store.put(&record).await.expect("put_then_get: put");
    let stored = store.get(&record.id).await.expect("put_then_get: get");
    assert_eq!(stored, record, "put_then_get: stored evidence differs");
    let missing: EvidenceId = id("ev-missing");
    assert_code(
        store.get(&missing).await,
        ErrorCode::NotFound,
        "put_then_get: missing",
    );
}

async fn identical_put_is_idempotent<S: EvidenceStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let record = evidence("ev-1", "run-1", at, "pass");
    store.put(&record).await.expect("idempotent: first put");
    store
        .put(&record)
        .await
        .expect("idempotent: identical put was rejected");
    let listed = store.list_for_run(&record.run_id).await.expect("list");
    assert_eq!(listed.len(), 1, "idempotent: stored twice");
}

async fn different_content_conflicts<S: EvidenceStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let original = evidence("ev-1", "run-1", at, "pass");
    store.put(&original).await.expect("conflict: first put");
    assert_code(
        store.put(&evidence("ev-1", "run-1", at, "fail")).await,
        ErrorCode::Conflict,
        "different_content_conflicts",
    );
    let stored = store.get(&original.id).await.expect("conflict: get");
    assert_eq!(
        stored, original,
        "different_content_conflicts: evidence was overwritten"
    );
}

async fn list_for_run_is_ordered_and_scoped<S: EvidenceStore>(store: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let later = evidence("ev-a", "run-1", at + TimeDelta::seconds(5), "pass");
    let same_time_b = evidence("ev-c", "run-1", at, "pass");
    let same_time_a = evidence("ev-b", "run-1", at, "fail");
    let other_run = evidence("ev-d", "run-2", at, "pass");
    for record in [&later, &same_time_b, &same_time_a, &other_run] {
        store.put(record).await.expect("list: put");
    }
    let listed = store.list_for_run(&id("run-1")).await.expect("list");
    let ids: Vec<&str> = listed.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["ev-b", "ev-c", "ev-a"],
        "list_for_run: order or scope"
    );
    let empty = store
        .list_for_run(&id::<RunId>("run-3"))
        .await
        .expect("list");
    assert!(
        empty.is_empty(),
        "list_for_run: evidence for an unknown run"
    );
}
