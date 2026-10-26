use std::sync::Arc;

use contracts::ids::{ProjectId, SdrVersion};
use contracts::{ErrorCode, SchemaSet, SdrViews};

use super::{assert_code, ctx};
use crate::builders::id;

const VIEW: &str = "api/sdr-version.schema.json";

/// What the SDR views suite tests: a project its implementation already
/// holds, with a version committed on evidence.
pub struct SdrViewsWorld {
    pub views: Arc<dyn SdrViews>,
    pub project: ProjectId,
    /// The project's latest version, whose record cites evidence.
    pub committed: SdrVersion,
}

/// The `SdrViews` contract.
pub async fn sdr_views(world: SdrViewsWorld) {
    let unknown: ProjectId = id("project-unknown");
    assert_code(
        world.views.version(&unknown, &SdrVersion(0), &ctx()).await,
        ErrorCode::NotFound,
        "sdr views: an unknown project",
    );
    let later = SdrVersion(world.committed.0 + 1);
    assert_code(
        world.views.version(&world.project, &later, &ctx()).await,
        ErrorCode::NotFound,
        "sdr views: a version not committed",
    );

    let view = world
        .views
        .version(&world.project, &world.committed, &ctx())
        .await
        .unwrap_or_else(|e| panic!("sdr views: a committed version: {e}"));
    assert_eq!(
        (&view.document.project_id, &view.document.version),
        (&world.project, &world.committed),
        "sdr views: the document asked for"
    );
    assert_eq!(
        (&view.record.project_id, &view.record.version),
        (&world.project, &world.committed),
        "sdr views: the document's own record"
    );
    assert!(
        !view.record.evidence_ids.is_empty(),
        "sdr views: the suite needs a version committed on evidence"
    );
    let served: Vec<&str> = view.evidence.iter().map(|e| e.id.as_str()).collect();
    let cited: Vec<&str> = view
        .record
        .evidence_ids
        .iter()
        .map(|e| e.as_str())
        .collect();
    assert_eq!(
        served, cited,
        "sdr views: the evidence the record cites, in order"
    );
    let schemas = SchemaSet::compile(&[VIEW]).expect("the view schema compiles");
    let problems = schemas.errors_of(VIEW, &view).expect("validate");
    assert!(
        problems.is_empty(),
        "sdr views: the view breaks its schema: {problems:?}"
    );
}
