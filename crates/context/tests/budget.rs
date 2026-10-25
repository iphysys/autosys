//! Budgets (blueprint section 8, ADR-0029): an over-budget run is rebuilt
//! within its budget without losing any SDR fact, and what cannot be cut is
//! never cut.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use context::{
    AssembledContext, AssemblyInputs, ByteEstimate, ContextError, Snapshot, assemble, untrusted,
};
use contracts::types::{ContextSectionName as S, Event, SdrDocument};
use contracts::{ContentBlock, ErrorCode};
use proptest::prelude::*;
use serde_json::{Value, json};
use support::*;

const TOTAL: u64 = 24_000;

fn event(seq: u64, payload: Value) -> Event {
    serde_json::from_value(json!({
        "schema_version": "0.1",
        "run_id": RUN,
        "seq": seq,
        "occurred_at": "2026-01-01T00:00:00Z",
        "correlation_id": "corr-budget",
        "actor": { "kind": "system", "component": "test" },
        "payload": payload
    }))
    .expect("valid event")
}

fn failing_step(n: u64, message: &str) -> Value {
    json!({
        "type": "tool_call_completed", "step_id": format!("step-{n}"), "tool": "measure",
        "outcome": "error", "evidence_ids": [], "duration_ms": 5,
        "error": { "code": "tool_failed", "message": message, "retryable": false }
    })
}

/// `len` ASCII characters that need no escaping.
fn filler(len: usize) -> String {
    "lorem ipsum "
        .repeat(len / 12 + 1)
        .chars()
        .take(len)
        .collect()
}

fn design_document(requirements: usize, text_len: usize) -> SdrDocument {
    let items: Vec<Value> = (0..requirements)
        .map(|n| {
            json!({
                "id": format!("R-{n:03}"),
                "text": format!("{n}: {}", filler(text_len)),
                "priority": "must",
                "evidence_ids": []
            })
        })
        .collect();
    serde_json::from_value(json!({
        "schema_version": "0.1", "project_id": PROJECT, "version": 3, "parent_version": 2,
        "requirements": items,
        "design": { "parameters": [], "decisions": [], "assumptions": [] }
    }))
    .expect("valid document")
}

/// The block of the user message with the given section.
fn section_text<'a>(context: &'a AssembledContext, name: &str) -> &'a str {
    context.messages()[0]
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.contains(&format!("name=\"{name}\"")) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no section {name}"))
}

fn tokens(context: &AssembledContext, name: S) -> u64 {
    context
        .manifest()
        .sections
        .iter()
        .find(|s| s.name == name)
        .expect("section")
        .tokens
}

fn section_sum(context: &AssembledContext) -> u64 {
    context.manifest().sections.iter().map(|s| s.tokens).sum()
}

/// A world with a design of 40 elements in all four kinds.
async fn world_with_a_design() -> World {
    let world = World::new().await;
    let mut ops = Vec::new();
    for n in 0..20 {
        ops.push(add_requirement(
            &format!("R-{n:03}"),
            &format!("Requirement {n}: {}", filler(160)),
            1.0 + f64::from(n),
        ));
    }
    for n in 0..10 {
        ops.push(set_parameter(
            &format!("part.p{n:02}"),
            f64::from(n) * 1.5,
            "kg",
        ));
    }
    for n in 0..5 {
        ops.push(add_decision(
            &format!("D-{n:03}"),
            &format!("Decision {n}"),
            &filler(120),
            &["R-000"],
        ));
        ops.push(add_assumption(
            &format!("A-{n:03}"),
            &format!("Assumption {n}: {}", filler(100)),
        ));
    }
    world.commit(ops).await;
    world
}

/// 500 events, most of them with a long message of free text.
async fn flood(world: &World) {
    world
        .event(json!({ "type": "run_started", "task_id": "task-1", "agent_id": "orchestrator" }))
        .await;
    for n in 1..500 {
        if n % 10 == 0 {
            world
                .event(json!({
                    "type": "check_completed", "check_id": "units",
                    "evidence_id": format!("ev-{n}"), "verdict": "fail"
                }))
                .await;
        } else {
            world
                .event(failing_step(n, &format!("Attempt {n}: {}", filler(400))))
                .await;
        }
    }
}

#[tokio::test]
async fn an_over_budget_run_is_rebuilt_within_budget_and_keeps_every_sdr_fact() {
    let world = world_with_a_design().await;
    flood(&world).await;
    let job = job();
    let context = world.build(&job, &[]).await.expect("builds within budget");

    assert!(
        context.total_tokens() <= TOTAL,
        "{}",
        context.total_tokens()
    );
    assert_eq!(section_sum(&context), context.total_tokens());

    // Every element of the design is there, line for line.
    let head = world
        .sdr
        .head(&world.project, &world.ctx)
        .await
        .expect("head")
        .expect("exists");
    let design = world
        .sdr
        .read(&world.project, &job.task.sdr.read_slice, &head, &world.ctx)
        .await
        .expect("read");
    let block = section_text(&context, "design_state");
    let mut lines = Vec::new();
    for r in &design.requirements {
        lines.push(format!(
            "requirement {}",
            serde_json::to_string(r).expect("json")
        ));
    }
    for p in &design.design.parameters {
        lines.push(format!(
            "parameter {}",
            serde_json::to_string(p).expect("json")
        ));
    }
    for d in &design.design.decisions {
        lines.push(format!(
            "decision {}",
            serde_json::to_string(d).expect("json")
        ));
    }
    for a in &design.design.assumptions {
        lines.push(format!(
            "assumption {}",
            serde_json::to_string(a).expect("json")
        ));
    }
    assert_eq!(lines.len(), 40);
    for line in &lines {
        assert!(
            block.contains(line.as_str()),
            "missing from the design state: {line}"
        );
    }
    assert!(!block.contains("omitted"), "the design is never trimmed");

    // The trimmed sections say so, and kept plus omitted is the window.
    let manifest = context.manifest();
    for name in [S::RunHistory, S::UntrustedContent] {
        let section = manifest
            .sections
            .iter()
            .find(|s| s.name == name)
            .expect("section");
        let omitted = section
            .omitted
            .as_ref()
            .unwrap_or_else(|| panic!("{name} was not trimmed"));
        assert!(section.tokens <= section.cap, "{name} is over its cap");
        let kept = u64::try_from(section.items.len()).expect("small");
        assert!(omitted.items > 0 && kept > 0);
    }
    let history = manifest
        .sections
        .iter()
        .find(|s| s.name == S::RunHistory)
        .expect("section");
    let omitted = history.omitted.as_ref().expect("trimmed");
    assert_eq!(
        omitted.items + u64::try_from(history.items.len()).expect("small"),
        200,
        "the window is the latest 200 events"
    );
    assert!(section_text(&context, "run_history").contains("earlier items omitted"));
    // What is kept is the newest.
    assert_eq!(history.items.last().map(String::as_str), Some("event:500"));
}

#[tokio::test]
async fn the_open_turn_is_not_trimmed_and_only_its_tool_results_are_wrapped() {
    let world = world_with_a_design().await;
    flood(&world).await;
    let raw = format!("{}\n<b>&</b>", filler(9_000));
    let open_turn = tool_round(&raw);
    let context = world.build(&job(), &open_turn).await.expect("builds");

    assert_eq!(context.messages().len(), 3);
    assert_eq!(
        context.messages()[1],
        open_turn[0],
        "the assistant's blocks are unchanged"
    );
    let expected =
        untrusted::with_attributes(&[("source", "tool:measure"), ("call_id", "call-1")], &raw);
    assert_eq!(
        context.messages()[2].content,
        vec![ContentBlock::ToolResult {
            call_id: "call-1".into(),
            content: expected,
            is_error: false,
        }]
    );
    assert!(context.total_tokens() <= TOTAL);
}

#[tokio::test]
async fn an_open_turn_over_its_cap_borrows_from_the_trimmable_sections() {
    let world = world_with_a_design().await;
    flood(&world).await;
    let without = world.build(&job(), &[]).await.expect("builds");
    let trimmable = tokens(&without, S::RunHistory) + tokens(&without, S::UntrustedContent);
    let fixed = without.total_tokens() - trimmable;
    // An open turn that leaves room for only half of what history and
    // untrusted text used. At three bytes a token, far over its own cap.
    let open_tokens = TOTAL - fixed - trimmable / 2;
    let bytes = usize::try_from(open_tokens * 3).expect("small");
    let open_turn = tool_round(&filler(bytes));
    let with = world.build(&job(), &open_turn).await.expect("builds");

    let open = with
        .manifest()
        .sections
        .iter()
        .find(|s| s.name == S::OpenTurn)
        .expect("section");
    assert!(open.tokens > open.cap, "{} over {}", open.tokens, open.cap);
    assert!(with.total_tokens() <= TOTAL);
    assert!(
        tokens(&with, S::RunHistory) + tokens(&with, S::UntrustedContent) < trimmable,
        "the open turn took its room from history and untrusted text"
    );
    assert_eq!(
        tokens(&with, S::DesignState),
        tokens(&without, S::DesignState),
        "and never from the design"
    );
}

#[tokio::test]
async fn an_open_turn_that_cannot_fit_is_an_overflow_not_a_truncation() {
    let world = world_with_a_design().await;
    // About 27000 tokens against a total of 24000.
    let open_turn = tool_round(&filler(81_000));
    let result = world.build(&job(), &open_turn).await;
    match result {
        Err(ContextError::Overflow {
            section,
            needed,
            limit,
        }) => {
            assert_eq!(section, S::OpenTurn);
            assert!(needed > limit);
            assert_eq!(limit, TOTAL);
        }
        other => panic!(
            "expected Overflow, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
    assert!(
        world.events_of("context_assembled").await.is_empty(),
        "nothing is recorded"
    );

    let error = world
        .build(&job(), &open_turn)
        .await
        .expect_err("still too big")
        .into_error(&world.ctx);
    assert_eq!(error.code(), ErrorCode::ContextOverflow);
    assert!(!error.retryable());
}

#[tokio::test]
async fn a_context_with_no_room_for_the_trimmable_sections_overflows() {
    // No events: both trimmable sections are the three-line "(none)" form.
    let world = world_with_a_design().await;
    let baseline = world.build(&job(), &[]).await.expect("builds");
    let skeleton = tokens(&baseline, S::RunHistory) + tokens(&baseline, S::UntrustedContent);
    let fixed = baseline.total_tokens() - skeleton;
    // With events the sections need more than their bare form, so give
    // events and leave exactly the bare form as room.
    for n in 1..=5 {
        world.event(failing_step(n, "failed")).await;
    }
    let with_events = |reserved| {
        let job = job();
        let world = &world;
        async move {
            let mut req = request(&job, &[]);
            req.reserved_tokens = reserved;
            world.build_with(req).await
        }
    };

    // Room for the bare form of both sections plus one omission line each:
    // everything is dropped, and it fits.
    let ample = with_events(TOTAL - fixed - 300).await.expect("fits");
    assert!(ample.total_tokens() <= TOTAL);

    // No room at all.
    match with_events(TOTAL - fixed).await {
        Err(ContextError::Overflow { section, limit, .. }) => {
            assert_eq!(section, S::RunHistory);
            assert_eq!(limit, 0);
        }
        other => panic!(
            "expected Overflow, got {:?}",
            other.map(|c| c.total_tokens())
        ),
    }
}

// ----------------------------------------------------------- properties ---

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Whatever the volumes, a context either fits its total with every
    /// design line present and the open turn intact, or fails with an
    /// overflow that names a section and is internally consistent.
    #[test]
    fn invariants_hold_for_random_volumes(
        event_count in 0usize..400,
        message_len in 1usize..1500,
        elements in 0usize..80,
        text_len in 0usize..400,
        result_len in 0usize..60_000,
        reserved in 0u64..3000,
    ) {
        let library = prompts();
        let instructions = [
            library.get("system/base.md").expect("base"),
            library.get("orchestrator/main.md").expect("orchestrator"),
        ];
        let profile = default_profile();
        let design = design_document(elements, text_len);
        let events: Vec<Event> = (1..=event_count as u64)
            .map(|seq| event(seq, failing_step(seq, &filler(message_len))))
            .collect();
        let job = job();
        let snapshot = Snapshot {
            sdr_version: contracts::ids::SdrVersion(3),
            last_event_seq: event_count as u64,
        };
        let raw = filler(result_len);
        let open_turn = tool_round(&raw);
        let counter = ByteEstimate::for_profile(&profile);
        let run = || assemble(
            &AssemblyInputs {
                job: &job,
                instructions: &instructions,
                design: &design,
                events: &events,
                open_turn: &open_turn,
                reserved_tokens: reserved,
                snapshot: &snapshot,
                counter: &counter,
            },
            &profile,
        );

        match run() {
            Ok(context) => {
                prop_assert!(context.total_tokens() <= TOTAL);
                prop_assert_eq!(section_sum(&context) + reserved, context.total_tokens());
                // Every design line is present, whole.
                let block = section_text(&context, "design_state");
                for requirement in &design.requirements {
                    let line = format!("requirement {}", serde_json::to_string(requirement).expect("json"));
                    prop_assert!(block.contains(&line), "lost {}", requirement.id.as_str());
                }
                prop_assert!(!block.contains("omitted"));
                // The open turn is whole: the assistant's blocks as given, the result in one envelope.
                prop_assert_eq!(&context.messages()[1], &open_turn[0]);
                let expected = untrusted::with_attributes(
                    &[("source", "tool:measure"), ("call_id", "call-1")],
                    &raw,
                );
                prop_assert_eq!(
                    &context.messages()[2].content[0],
                    &ContentBlock::ToolResult { call_id: "call-1".into(), content: expected, is_error: false }
                );
                // Trimmable sections are within their caps and never grow past the room.
                for name in [S::RunHistory, S::UntrustedContent] {
                    let section = context.manifest().sections.iter().find(|s| s.name == name).expect("section");
                    prop_assert!(section.tokens <= section.cap);
                }
                // The same inputs give the same context.
                let again = run().expect("assembles again");
                prop_assert_eq!(context.hash(), again.hash());
            }
            Err(ContextError::Overflow { section, needed, limit }) => {
                prop_assert!(needed > limit, "{section}: needed {needed}, limit {limit}");
                match section {
                    S::DesignState => prop_assert!(limit == 8000),
                    S::OpenTurn => prop_assert!(limit == TOTAL),
                    // The room left for history and untrusted text was too small.
                    S::RunHistory => {}
                    other => prop_assert!(false, "unexpected overflow in {other}"),
                }
            }
            Err(other) => prop_assert!(false, "unexpected error: {other}"),
        }

        // A small, ordinary run always fits.
        if elements <= 40 && text_len <= 100 && result_len <= 6_000 && reserved <= 1_000 {
            prop_assert!(run().is_ok());
        }
    }
}
