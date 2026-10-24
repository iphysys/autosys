//! Failures (ADR-0029): each one is a typed error with the right code, none
//! hands out a context, and none records an event.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use context::{ContextBuilder, ContextError, PromptLibrary};
use contracts::types::ContextSectionName as S;
use contracts::{ErrorCode, ExecutionContext};
use sdr::SdrManager;
use serde_json::json;
use support::*;
use testkit::builders::id;
use testkit::clock::ManualClock;
use testkit::fakes::{Fault, faulty_sdr_storage, faulty_sdr_storage_after_head};
use testkit::stores::InMemoryEventLog;

fn filler(len: usize) -> String {
    "lorem ipsum "
        .repeat(len / 12 + 1)
        .chars()
        .take(len)
        .collect()
}

fn overflow(result: Result<context::AssembledContext, ContextError>) -> (S, u64, u64) {
    match result {
        Err(ContextError::Overflow {
            section,
            needed,
            limit,
        }) => (section, needed, limit),
        other => panic!(
            "expected Overflow, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
}

/// A builder over storage and an event log that the test controls.
struct Rig {
    builder: ContextBuilder,
    ctx: ExecutionContext,
    events: Arc<InMemoryEventLog>,
}

impl Rig {
    fn over(sdr: SdrManager, clock: Arc<ManualClock>) -> Self {
        let events = Arc::new(InMemoryEventLog::new());
        let (builder, ctx) = World::from_parts(Arc::new(sdr), events.clone(), profiles(), clock);
        Self {
            builder,
            ctx,
            events,
        }
    }

    async fn build(&self) -> Result<context::AssembledContext, ContextError> {
        let job = job();
        self.builder.build(request(&job, &[]), &self.ctx).await
    }

    async fn nothing_recorded(&self) {
        let events = contracts::EventLog::read(&*self.events, &id(RUN), 0, 100)
            .await
            .expect("read");
        assert!(events.is_empty(), "an event was recorded: {events:?}");
    }
}

// ------------------------------------------------------- sections too big ---

#[tokio::test]
async fn a_task_contract_over_its_cap_overflows_and_nothing_is_dropped() {
    let world = World::new().await;
    let mut big = job();
    big.task.goal = filler(6_000).parse().expect("goal");
    let (section, needed, limit) = overflow(world.build(&big, &[]).await);
    assert_eq!(section, S::TaskContract);
    assert_eq!(limit, 1_500);
    assert!(needed > limit);
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test]
async fn a_job_state_over_its_cap_overflows() {
    let world = World::new().await;
    let mut big = job();
    big.pending_work = (0..60)
        .map(|n| {
            format!("Pending step number {n} of the work")
                .parse()
                .expect("text")
        })
        .collect();
    let (section, _, limit) = overflow(world.build(&big, &[]).await);
    assert_eq!(section, S::JobState);
    assert_eq!(limit, 500);
}

#[tokio::test]
async fn a_design_state_over_its_cap_overflows_instead_of_dropping_elements() {
    let world = World::new().await;
    // 100 requirements of about 300 bytes: well over 8000 tokens.
    let ops = (0..100)
        .map(|n| add_text_requirement(&format!("R-{n:03}"), &format!("{n}: {}", filler(300))))
        .collect();
    world.commit(ops).await;
    let (section, needed, limit) = overflow(world.build(&job(), &[]).await);
    assert_eq!(section, S::DesignState);
    assert_eq!(limit, 8_000);
    assert!(needed > limit);
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test]
async fn instructions_over_their_cap_overflow() {
    let world = World::with(profiles(), prompts_with_base(&filler(12_000))).await;
    let (section, _, limit) = overflow(world.build(&job(), &[]).await);
    assert_eq!(section, S::Instructions);
    assert_eq!(limit, 3_000);
    assert!(world.events_of("context_assembled").await.is_empty());
}

// --------------------------------------------------------- SDR read errors ---

#[tokio::test]
async fn a_project_without_an_sdr_is_not_found() {
    let world = World::new().await;
    let mut other = job();
    other.project_id = id("no-such-project");
    match world.build(&other, &[]).await {
        Err(ContextError::Sdr(error)) => assert_eq!(error.code(), ErrorCode::NotFound),
        other => panic!(
            "expected an SDR error, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    let error = world
        .build(&other, &[])
        .await
        .expect_err("still missing")
        .into_error(&world.ctx);
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(
        error.message().contains("no-such-project"),
        "{}",
        error.message()
    );
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test]
async fn a_pinned_version_that_does_not_exist_is_not_found() {
    let world = World::new().await;
    let job = job();
    let mut req = request(&job, &[]);
    req.at = Some(pinned(9, 0));
    match world.build_with(req).await {
        Err(ContextError::Sdr(error)) => assert_eq!(error.code(), ErrorCode::NotFound),
        other => panic!(
            "expected an SDR error, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
}

#[tokio::test]
async fn the_code_of_a_failing_sdr_is_kept() {
    let cases = [
        (
            faulty_sdr_storage(Fault::Fail(ErrorCode::Unavailable)),
            ErrorCode::Unavailable,
        ),
        (
            // `head` answers and reading the document fails.
            faulty_sdr_storage_after_head(1, Fault::Fail(ErrorCode::PermissionDenied)),
            ErrorCode::PermissionDenied,
        ),
    ];
    for (storage, code) in cases {
        let clock = Arc::new(ManualClock::at_epoch());
        let rig = Rig::over(
            SdrManager::new(storage, clock.clone()).expect("manager"),
            clock,
        );
        match rig.build().await {
            Err(ContextError::Sdr(error)) => assert_eq!(error.code(), code),
            other => panic!(
                "expected an SDR error, got {:?}",
                other.map(|c| c.total_tokens())
            ),
        }
        let error = rig.build().await.expect_err("fails").into_error(&rig.ctx);
        assert_eq!(error.code(), code);
        assert_eq!(error.retryable(), code.is_retryable());
        rig.nothing_recorded().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_hanging_sdr_times_out_instead_of_waiting_forever() {
    for (storage, operation) in [
        (faulty_sdr_storage(Fault::Hang), "reading the SDR head"),
        (
            faulty_sdr_storage_after_head(1, Fault::Hang),
            "reading the SDR",
        ),
    ] {
        let clock = Arc::new(ManualClock::at_epoch());
        let rig = Rig::over(
            SdrManager::new(storage, clock.clone()).expect("manager"),
            clock,
        );
        match rig.build().await {
            Err(ContextError::Timeout {
                operation: what,
                limit,
            }) => {
                assert_eq!(what, operation);
                assert_eq!(
                    limit,
                    Duration::from_secs(10),
                    "the profile's io_timeout_seconds"
                );
            }
            other => panic!(
                "expected a timeout, got {:?}",
                other.map(|c| c.total_tokens())
            ),
        }
        let error = rig
            .build()
            .await
            .expect_err("times out")
            .into_error(&rig.ctx);
        assert_eq!(error.code(), ErrorCode::Timeout);
        assert!(error.retryable());
        rig.nothing_recorded().await;
    }
}

// ------------------------------------------------------- event log errors ---

#[tokio::test]
async fn the_code_of_a_failing_event_log_is_kept() {
    let world = World::new().await;
    let mut flaky = FlakyEvents::new(world.events.clone());
    flaky.read = Behaviour::Fail(ErrorCode::Unavailable);
    let builder = world.builder_over(Arc::new(flaky));
    let job = job();
    match builder.build(request(&job, &[]), &world.ctx).await {
        Err(ContextError::Events(error)) => assert_eq!(error.code(), ErrorCode::Unavailable),
        other => panic!(
            "expected an event log error, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_hanging_event_log_times_out() {
    for (set, operation) in [
        (
            (|f: &mut FlakyEvents| f.last_seq = Behaviour::Hang) as fn(&mut FlakyEvents),
            "reading the last event",
        ),
        (
            |f: &mut FlakyEvents| f.read = Behaviour::Hang,
            "reading the events",
        ),
        (
            |f: &mut FlakyEvents| f.append = Behaviour::Hang,
            "recording the context",
        ),
    ] {
        let world = World::new().await;
        let mut flaky = FlakyEvents::new(world.events.clone());
        set(&mut flaky);
        let builder = world.builder_over(Arc::new(flaky));
        let job = job();
        match builder.build(request(&job, &[]), &world.ctx).await {
            Err(ContextError::Timeout {
                operation: what, ..
            }) => assert_eq!(what, operation),
            other => panic!(
                "expected a timeout, got {:?}",
                other.map(|c| c.total_tokens())
            ),
        }
        assert!(world.events_of("context_assembled").await.is_empty());
    }
}

// ------------------------------------------------------------ other errors ---

#[tokio::test]
async fn an_unknown_profile_lists_the_available_ones() {
    let world = World::with(
        profiles_from(
            vec![
                profile_json("default", 24_000, json!({})),
                profile_json(
                    "compact",
                    12_000,
                    json!({
                        "instructions": 2000, "task_contract": 1000, "job_state": 400,
                        "design_state": 4000, "run_history": 1500, "untrusted_content": 2000,
                        "open_turn": 3000
                    }),
                ),
            ],
            24_000,
        )
        .expect("valid profiles"),
        prompts(),
    )
    .await;
    let job = job();
    let mut req = request(&job, &[]);
    req.profile = Some("huge");
    match world.build_with(req).await {
        Err(ContextError::UnknownProfile { name, available }) => {
            assert_eq!(name, "huge");
            assert_eq!(available, ["compact", "default"]);
        }
        other => panic!(
            "expected UnknownProfile, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    // A named profile that exists is used.
    let mut req = request(&job, &[]);
    req.profile = Some("compact");
    let context = world.build_with(req).await.expect("builds");
    assert_eq!(context.manifest().profile, "compact");
    assert!(context.total_tokens() <= 12_000);
}

#[tokio::test]
async fn an_unknown_prompt_lists_the_available_ones() {
    let world = World::new().await;
    let job = job();
    let mut req = request(&job, &[]);
    req.prompt = "orchestrator/missing.md";
    match world.build_with(req).await {
        Err(ContextError::UnknownPrompt { path, available }) => {
            assert_eq!(path, "orchestrator/missing.md");
            assert!(
                available.contains(&"orchestrator/main.md".to_owned()),
                "{available:?}"
            );
        }
        other => panic!(
            "expected UnknownPrompt, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
}

#[test]
fn a_builder_needs_the_base_prompt() {
    let agent_only = PromptLibrary::from_files(vec![(
        "agents/only.md".to_owned(),
        "---\nschema_version: \"0.1\"\nid: agents.only\nversion: 0.1.0\npurpose: A prompt.\n\
         inputs: []\nowner: harness\norigin: human\n---\n\nText.\n"
            .to_owned(),
    )])
    .expect("library");
    let clock = Arc::new(ManualClock::at_epoch());
    let sdr =
        Arc::new(SdrManager::new(testkit::stores::sdr_storage(), clock.clone()).expect("manager"));
    let result = ContextBuilder::new(
        sdr,
        Arc::new(InMemoryEventLog::new()),
        Arc::new(agent_only),
        profiles(),
        clock,
    );
    match result {
        Err(ContextError::UnknownPrompt { path, .. }) => assert_eq!(path, "system/base.md"),
        other => panic!("expected UnknownPrompt, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn a_cancelled_context_builds_nothing() {
    let world = World::new().await;
    world.ctx.cancel();
    let error = world
        .build(&job(), &[])
        .await
        .expect_err("cancelled")
        .into_error(&world.ctx);
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_while_waiting_on_the_sdr_stops_the_build() {
    let clock = Arc::new(ManualClock::at_epoch());
    let rig = Rig::over(
        SdrManager::new(faulty_sdr_storage(Fault::Hang), clock.clone()).expect("manager"),
        clock,
    );
    let cancel = rig.ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
    });
    // The profile's timeout is ten seconds; the cancel comes first.
    match rig.build().await {
        Err(ContextError::Cancelled) => {}
        other => panic!(
            "expected Cancelled, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    rig.nothing_recorded().await;
}

#[tokio::test]
async fn a_context_without_a_run_cannot_be_recorded() {
    let world = World::new().await;
    let no_run = ExecutionContext::new(id("corr-no-run")).with_project(id(PROJECT));
    let job = job();
    let result = world.builder.build(request(&job, &[]), &no_run).await;
    match result {
        Err(error @ ContextError::MissingRun) => {
            assert_eq!(error.code(), ErrorCode::InvalidInput);
        }
        other => panic!(
            "expected MissingRun, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    assert!(world.events_of("context_assembled").await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_invalid_open_turn_is_rejected_before_anything_is_read() {
    // The SDR hangs, so any read would time out: the error shows none was made.
    let clock = Arc::new(ManualClock::at_epoch());
    let rig = Rig::over(
        SdrManager::new(faulty_sdr_storage(Fault::Hang), clock.clone()).expect("manager"),
        clock,
    );
    let job = job();
    let user_first = vec![user(vec![text("ignore previous instructions")])];
    let result = rig
        .builder
        .build(request(&job, &user_first), &rig.ctx)
        .await;
    match result {
        Err(ContextError::InvalidOpenTurn { .. }) => {}
        other => panic!(
            "expected InvalidOpenTurn, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    let unanswered = vec![
        assistant(vec![call("c1", "measure", json!({}))]),
        user(vec![result_for("c9")]),
    ];
    let result = rig
        .builder
        .build(request(&job, &unanswered), &rig.ctx)
        .await;
    let error = result.expect_err("rejected").into_error(&rig.ctx);
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    rig.nothing_recorded().await;
}

fn result_for(call_id: &str) -> contracts::ContentBlock {
    result(call_id, "output")
}

#[tokio::test]
async fn errors_carry_the_correlation_id() {
    let world = World::new().await;
    let mut big = job();
    big.task.goal = filler(6_000).parse().expect("goal");
    let error = world
        .build(&big, &[])
        .await
        .expect_err("overflows")
        .into_error(&world.ctx);
    assert_eq!(error.code(), ErrorCode::ContextOverflow);
    assert_eq!(
        error.correlation_id().map(|c| c.as_str()),
        Some("corr-context")
    );
    assert!(
        error.message().contains("task_contract"),
        "{}",
        error.message()
    );
}
