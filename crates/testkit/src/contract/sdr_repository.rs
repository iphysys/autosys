use std::future::Future;

use chrono::{TimeZone, Utc};
use contracts::ids::{ProjectId, SdrVersion};
use contracts::{ErrorCode, SdrRepository};

use super::assert_code;
use crate::builders::{id, sdr_document, version_record};

/// The `SdrRepository` contract. `make` builds an empty repository.
pub async fn sdr_repository<S, F, Fut>(make: F)
where
    S: SdrRepository,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    empty_project_has_no_head(make().await).await;
    versions_append_in_order(make().await).await;
    first_version_must_be_zero(make().await).await;
    stale_or_skipped_versions_conflict(make().await).await;
    document_and_record_must_agree(make().await).await;
    projects_are_independent(make().await).await;
}

async fn append<S: SdrRepository>(repo: &S, project: &str, version: u64, parent: Option<u64>) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    repo.append_version(
        &sdr_document(project, version, parent),
        &version_record(project, version, parent, at),
    )
    .await
    .unwrap_or_else(|e| panic!("append version {version} of {project}: {e}"));
}

async fn empty_project_has_no_head<S: SdrRepository>(repo: S) {
    let project: ProjectId = id("p1");
    assert_eq!(
        repo.head(&project).await.expect("head"),
        None,
        "empty_project: head"
    );
    assert_code(
        repo.document(&project, &SdrVersion(0)).await,
        ErrorCode::NotFound,
        "empty_project_has_no_head: document",
    );
    assert_code(
        repo.version_record(&project, &SdrVersion(0)).await,
        ErrorCode::NotFound,
        "empty_project_has_no_head: version record",
    );
}

async fn versions_append_in_order<S: SdrRepository>(repo: S) {
    let project: ProjectId = id("p1");
    append(&repo, "p1", 0, None).await;
    assert_eq!(
        repo.head(&project).await.expect("head"),
        Some(SdrVersion(0)),
        "append: head after v0"
    );
    append(&repo, "p1", 1, Some(0)).await;
    assert_eq!(
        repo.head(&project).await.expect("head"),
        Some(SdrVersion(1)),
        "append: head after v1"
    );

    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for (version, parent) in [(0, None), (1, Some(0))] {
        let doc = repo
            .document(&project, &SdrVersion(version))
            .await
            .expect("document");
        assert_eq!(
            doc,
            sdr_document("p1", version, parent),
            "append: document {version} differs"
        );
        let record = repo
            .version_record(&project, &SdrVersion(version))
            .await
            .expect("version_record");
        assert_eq!(
            record,
            version_record("p1", version, parent, at),
            "append: record {version} differs"
        );
    }
}

async fn first_version_must_be_zero<S: SdrRepository>(repo: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    assert_code(
        repo.append_version(
            &sdr_document("p1", 1, Some(0)),
            &version_record("p1", 1, Some(0), at),
        )
        .await,
        ErrorCode::Conflict,
        "first_version_must_be_zero",
    );
    assert_eq!(
        repo.head(&id("p1")).await.expect("head"),
        None,
        "first_version: head changed"
    );
}

async fn stale_or_skipped_versions_conflict<S: SdrRepository>(repo: S) {
    append(&repo, "p1", 0, None).await;
    append(&repo, "p1", 1, Some(0)).await;
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for (version, parent, case) in [
        (1, Some(0), "stale: version 1 again"),
        (2, Some(0), "stale: parent is not the head"),
        (3, Some(2), "skipped: version after a missing one"),
    ] {
        assert_code(
            repo.append_version(
                &sdr_document("p1", version, parent),
                &version_record("p1", version, parent, at),
            )
            .await,
            ErrorCode::Conflict,
            case,
        );
    }
    assert_eq!(
        repo.head(&id("p1")).await.expect("head"),
        Some(SdrVersion(1)),
        "stale: head changed"
    );
}

async fn document_and_record_must_agree<S: SdrRepository>(repo: S) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for (document, record, case) in [
        (
            sdr_document("p1", 0, None),
            version_record("p1", 1, None, at),
            "version mismatch",
        ),
        (
            sdr_document("p1", 0, None),
            version_record("p2", 0, None, at),
            "project mismatch",
        ),
        (
            sdr_document("p1", 0, None),
            version_record("p1", 0, Some(0), at),
            "parent mismatch",
        ),
    ] {
        assert_code(
            repo.append_version(&document, &record).await,
            ErrorCode::InvalidInput,
            &format!("document_and_record_must_agree: {case}"),
        );
    }
    assert_eq!(
        repo.head(&id("p1")).await.expect("head"),
        None,
        "must_agree: head changed"
    );
}

async fn projects_are_independent<S: SdrRepository>(repo: S) {
    append(&repo, "p1", 0, None).await;
    append(&repo, "p1", 1, Some(0)).await;
    append(&repo, "p2", 0, None).await;
    assert_eq!(
        repo.head(&id("p2")).await.expect("head"),
        Some(SdrVersion(0)),
        "independent: p2 head"
    );
    assert_eq!(
        repo.head(&id("p1")).await.expect("head"),
        Some(SdrVersion(1)),
        "independent: p1 head"
    );
}
