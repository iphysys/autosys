use std::future::Future;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use contracts::ids::{ProjectId, SdrVersion};
use contracts::{Clock, ErrorCode, JobStore, SdrRepository, WriteAuthority};

use super::{assert_code, fenced, lease_one};
use crate::builders::{id, sdr_document, version_record};
use crate::clock::ManualClock;

struct Fixture<S> {
    repo: S,
    /// The fence of a job claimed for the whole case.
    authority: WriteAuthority,
}

async fn fresh<S, J, F, Fut>(make: &F) -> Fixture<S>
where
    J: JobStore,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = (S, J)>,
{
    let clock = Arc::new(ManualClock::at_epoch());
    let (repo, jobs) = make(clock.clone()).await;
    let claimed = lease_one(&jobs, &clock).await;
    Fixture {
        repo,
        authority: fenced(&claimed),
    }
}

/// The `SdrRepository` contract. `make` builds an empty repository, and a
/// job store on the given clock whose fences it checks. Every append carries
/// a current fence; the fencing suite covers stale ones and the unfenced
/// version 0.
pub async fn sdr_repository<S, J, F, Fut>(make: F)
where
    S: SdrRepository,
    J: JobStore,
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = (S, J)>,
{
    empty_project_has_no_head(fresh(&make).await).await;
    versions_append_in_order(fresh(&make).await).await;
    first_version_must_be_zero(fresh(&make).await).await;
    stale_or_skipped_versions_conflict(fresh(&make).await).await;
    document_and_record_must_agree(fresh(&make).await).await;
    projects_are_independent(fresh(&make).await).await;
}

async fn append<S: SdrRepository>(
    f: &Fixture<S>,
    project: &str,
    version: u64,
    parent: Option<u64>,
) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    f.repo
        .append_version(
            &sdr_document(project, version, parent),
            &version_record(project, version, parent, at),
            Some(&f.authority),
        )
        .await
        .unwrap_or_else(|e| panic!("append version {version} of {project}: {e}"));
}

async fn empty_project_has_no_head<S: SdrRepository>(f: Fixture<S>) {
    let repo = &f.repo;
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

async fn versions_append_in_order<S: SdrRepository>(f: Fixture<S>) {
    let repo = &f.repo;
    let project: ProjectId = id("p1");
    append(&f, "p1", 0, None).await;
    assert_eq!(
        repo.head(&project).await.expect("head"),
        Some(SdrVersion(0)),
        "append: head after v0"
    );
    append(&f, "p1", 1, Some(0)).await;
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

async fn first_version_must_be_zero<S: SdrRepository>(f: Fixture<S>) {
    let (repo, authority) = (&f.repo, Some(&f.authority));
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    assert_code(
        repo.append_version(
            &sdr_document("p1", 1, Some(0)),
            &version_record("p1", 1, Some(0), at),
            authority,
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

async fn stale_or_skipped_versions_conflict<S: SdrRepository>(f: Fixture<S>) {
    let (repo, authority) = (&f.repo, Some(&f.authority));
    append(&f, "p1", 0, None).await;
    append(&f, "p1", 1, Some(0)).await;
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
                authority,
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

async fn document_and_record_must_agree<S: SdrRepository>(f: Fixture<S>) {
    let repo = &f.repo;
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
            repo.append_version(&document, &record, Some(&f.authority))
                .await,
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

async fn projects_are_independent<S: SdrRepository>(f: Fixture<S>) {
    let repo = &f.repo;
    append(&f, "p1", 0, None).await;
    append(&f, "p1", 1, Some(0)).await;
    append(&f, "p2", 0, None).await;
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
