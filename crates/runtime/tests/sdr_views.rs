//! `runtime::Views` keeps the `SdrViews` contract (ADR-0035) on a project a
//! run committed to.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use contracts::ids::SdrVersion;
use contracts::types::StopReason;
use runtime::Views;
use support::*;
use testkit::builders::id;
use testkit::contract::{SdrViewsWorld, sdr_views};

#[tokio::test]
async fn the_views_of_a_committed_run_keep_the_contract() {
    let world = World::new(Setup::new(vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        call_tools(vec![("c2", "measure", measure_args())]),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[(
                "c3".to_owned(),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, true),
            )])
        }),
    ]))
    .await;
    let ran = world
        .run(job_for(task_json(&["R-mass"], roomy_budget())))
        .await;
    assert_eq!(ran.outcome().stop_reason, StopReason::Success);
    assert_eq!(world.head().await, 2);

    sdr_views(SdrViewsWorld {
        views: Arc::new(Views::new(
            world.sdr.clone(),
            world.evidence.clone(),
            Duration::from_secs(5),
        )),
        project: id(PROJECT),
        committed: SdrVersion(2),
    })
    .await;
}
