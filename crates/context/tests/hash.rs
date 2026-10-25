//! The context hash (ADR-0029): the same inputs give the same hash, and a
//! change to anything the model sees gives a different one.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::time::Duration;

use context::{AssemblyInputs, ByteEstimate, Snapshot, assemble};
use contracts::types::{Event, Job, SdrDocument};
use contracts::{ContentBlock, Message};
use serde_json::json;
use support::*;

/// Gives a world a small design and a few events, the same every time.
async fn populate(world: &World) {
    world
        .commit(vec![
            add_requirement("R-001", "The plate mass shall not exceed 1 kg.", 1.0),
            add_text_requirement("R-002", "The plate shall fit a 120 mm by 80 mm envelope."),
        ])
        .await;
    world
        .commit(vec![
            set_parameter("part.mass", 0.84, "kg"),
            add_assumption("A-001", "The plate is aluminium 6061."),
            add_decision(
                "D-001",
                "Use a 4 mm plate.",
                "Keeps the mass in budget.",
                &["R-001"],
            ),
        ])
        .await;
    world
        .event(json!({ "type": "run_started", "task_id": "task-1", "agent_id": "orchestrator" }))
        .await;
    world
        .event(json!({
            "type": "tool_call_completed", "step_id": "step-1", "tool": "measure",
            "outcome": "error", "evidence_ids": [], "duration_ms": 5,
            "error": { "code": "timeout", "message": "No answer.", "retryable": true }
        }))
        .await;
}

async fn populated() -> World {
    let world = World::new().await;
    populate(&world).await;
    world
}

async fn hash_of(world: &World, job: &Job, open_turn: &[Message]) -> String {
    world
        .build(job, open_turn)
        .await
        .expect("builds")
        .hash()
        .as_str()
        .to_owned()
}

#[tokio::test]
async fn the_same_inputs_give_the_same_hash() {
    let world = populated().await;
    let open_turn = tool_round("mass = 0.84 kg");
    let first = hash_of(&world, &job(), &open_turn).await;
    // The first build appended a `context_assembled` event. It is not in the
    // history, so the second build sees the same thing.
    let second = hash_of(&world, &job(), &open_turn).await;
    assert_eq!(first, second);

    let other_builder = populated().await;
    assert_eq!(first, hash_of(&other_builder, &job(), &open_turn).await);
}

#[tokio::test]
async fn the_clock_and_the_jobs_bookkeeping_do_not_matter() {
    let open_turn = tool_round("mass = 0.84 kg");
    let a = populated().await;
    let b = populated().await;
    b.clock.advance(Duration::from_secs(86_400 * 30));

    let mut moved = job();
    moved.attempt = 7;
    moved.updated_at = "2027-06-01T00:00:00Z".parse().expect("timestamp");
    moved.idempotency_key = "another-key".parse().expect("key");
    assert_eq!(
        hash_of(&a, &job(), &open_turn).await,
        hash_of(&b, &moved, &open_turn).await,
        "time, attempt and keys are not part of the context"
    );
}

#[tokio::test]
async fn events_the_model_does_not_see_do_not_change_the_hash() {
    let world = populated().await;
    let before = hash_of(&world, &job(), &[]).await;
    world
        .event(json!({
            "type": "model_call_completed", "model_profile": "haiku", "model_id": "m",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }))
        .await;
    world
        .event(json!({
            "type": "tool_call_started", "step_id": "step-2", "tool": "measure",
            "idempotency_key": "k"
        }))
        .await;
    world
        .event(
            json!({ "type": "job_state_changed", "from": "queued", "to": { "state": "running" } }),
        )
        .await;
    assert_eq!(before, hash_of(&world, &job(), &[]).await);

    // An event the model does see does.
    world
        .event(json!({
            "type": "check_completed", "check_id": "units", "evidence_id": "ev-1", "verdict": "pass"
        }))
        .await;
    assert_ne!(before, hash_of(&world, &job(), &[]).await);
}

/// Everything `assemble` takes, read from a world.
struct Inputs {
    job: Job,
    design: SdrDocument,
    events: Vec<Event>,
    snapshot: Snapshot,
}

async fn inputs_of(world: &World) -> Inputs {
    let job = job();
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
    let events = world.all_events().await;
    let snapshot = Snapshot {
        sdr_version: head,
        last_event_seq: events.last().map_or(0, |e| e.seq.get()),
    };
    Inputs {
        job,
        design,
        events,
        snapshot,
    }
}

fn assemble_hash(inputs: &Inputs, open_turn: &[Message]) -> String {
    let library = prompts();
    let instructions = [
        library.get("system/base.md").expect("base"),
        library.get("orchestrator/main.md").expect("orchestrator"),
    ];
    let profile = default_profile();
    assemble(
        &AssemblyInputs {
            job: &inputs.job,
            instructions: &instructions,
            design: &inputs.design,
            events: &inputs.events,
            open_turn,
            reserved_tokens: 0,
            snapshot: &inputs.snapshot,
            counter: &ByteEstimate::for_profile(&profile),
        },
        &profile,
    )
    .expect("assembles")
    .hash()
    .as_str()
    .to_owned()
}

#[tokio::test]
async fn assembly_is_pure_and_ignores_the_order_of_elements_and_events() {
    let world = populated().await;
    let inputs = inputs_of(&world).await;
    let open_turn = tool_round("mass = 0.84 kg");
    let baseline = assemble_hash(&inputs, &open_turn);
    assert_eq!(baseline, assemble_hash(&inputs, &open_turn), "twice");

    let mut shuffled = Inputs {
        job: inputs.job.clone(),
        design: inputs.design.clone(),
        events: inputs.events.clone(),
        snapshot: inputs.snapshot.clone(),
    };
    shuffled.design.requirements.reverse();
    shuffled.design.design.parameters.reverse();
    shuffled.design.design.decisions.reverse();
    shuffled.design.design.assumptions.reverse();
    shuffled.events.reverse();
    assert_eq!(baseline, assemble_hash(&shuffled, &open_turn));

    // Events after the snapshot are not seen, however many are passed in.
    let mut with_later = Inputs {
        job: inputs.job.clone(),
        design: inputs.design.clone(),
        events: inputs.events.clone(),
        snapshot: inputs.snapshot.clone(),
    };
    world
        .event(json!({ "type": "cancel_requested", "reason": "later" }))
        .await;
    with_later.events = world.all_events().await;
    assert_eq!(baseline, assemble_hash(&with_later, &open_turn));
}

#[tokio::test]
async fn the_order_of_keys_in_a_tool_call_does_not_matter() {
    let world = populated().await;
    let inputs = inputs_of(&world).await;
    let turn = |arguments: &str| {
        let arguments: serde_json::Value = serde_json::from_str(arguments).expect("json");
        vec![
            assistant(vec![call("c1", "measure", arguments)]),
            user(vec![result("c1", "ok")]),
        ]
    };
    assert_eq!(
        assemble_hash(&inputs, &turn(r#"{"a":1,"b":{"x":1,"y":2}}"#)),
        assemble_hash(&inputs, &turn(r#"{"b":{"y":2,"x":1},"a":1}"#)),
    );
}

#[tokio::test]
async fn one_changed_byte_anywhere_changes_the_hash() {
    let open_turn = tool_round("mass = 0.84 kg");
    let baseline = hash_of(&populated().await, &job(), &open_turn).await;
    let mut seen = vec![baseline.clone()];
    let mut check = |name: &str, hash: String| {
        assert!(
            !seen.contains(&hash),
            "changing {name} did not change the hash"
        );
        seen.push(hash);
    };

    // The design.
    let world = populated().await;
    world
        .commit(vec![set_parameter("part.mass", 0.85, "kg")])
        .await;
    check("the SDR", hash_of(&world, &job(), &open_turn).await);

    // An event.
    let world = populated().await;
    world
        .event(json!({
            "type": "tool_call_completed", "step_id": "step-1", "tool": "measure",
            "outcome": "error", "evidence_ids": [], "duration_ms": 5,
            "error": { "code": "timeout", "message": "No answer!", "retryable": true }
        }))
        .await;
    check("an event", hash_of(&world, &job(), &open_turn).await);

    // The open turn: a tool result, and a tool call's arguments.
    check(
        "a tool result",
        hash_of(&populated().await, &job(), &tool_round("mass = 0.85 kg")).await,
    );
    let mut other_args = tool_round("mass = 0.84 kg");
    other_args[0].content[2] = call("call-1", "measure", json!({ "part": "plate", "unit": "g" }));
    check(
        "a tool call",
        hash_of(&populated().await, &job(), &other_args).await,
    );
    let mut other_thinking = tool_round("mass = 0.84 kg");
    other_thinking[0].content[0] = thinking("The mass needs measuring!");
    check(
        "a thinking block",
        hash_of(&populated().await, &job(), &other_thinking).await,
    );

    // The task.
    let mut other_job = job();
    other_job.task.goal = "Size a mounting plate that meets its mass requirement."
        .parse()
        .expect("goal");
    check(
        "the goal",
        hash_of(&populated().await, &other_job, &open_turn).await,
    );
    let mut other_progress = job();
    other_progress.progress.steps_completed += 1;
    check(
        "the job's progress",
        hash_of(&populated().await, &other_progress, &open_turn).await,
    );

    // A prompt, by one character.
    let base = prompts()
        .get("system/base.md")
        .expect("base")
        .body()
        .to_owned();
    let changed = World::with(profiles(), prompts_with_base(&format!("{base}."))).await;
    populate(&changed).await;
    check("a prompt", hash_of(&changed, &job(), &open_turn).await);

    // The agent's prompt.
    let world = populated().await;
    let base_job = job();
    let mut other_prompt = request(&base_job, &open_turn);
    other_prompt.prompt = "system/base.md";
    let hash = world
        .build_with(other_prompt)
        .await
        .expect("builds")
        .hash()
        .as_str()
        .to_owned();
    check("the agent's prompt", hash);
}

#[tokio::test]
async fn the_hash_is_the_one_in_the_event_and_the_request_carries_what_was_hashed() {
    let world = populated().await;
    let context = world.build(&job(), &[]).await.expect("builds");
    let events = world.events_of("context_assembled").await;
    assert_eq!(events.len(), 1);
    let payload = serde_json::to_value(&events[0].payload).expect("payload");
    assert_eq!(payload["context_hash"], context.hash().as_str());

    let request = context
        .clone()
        .into_model_request("m".into(), Vec::new(), 100);
    assert_eq!(request.system, context.system());
    assert_eq!(request.messages, context.messages());
}

#[tokio::test]
async fn a_tool_error_flag_is_part_of_the_hash() {
    let world = populated().await;
    let inputs = inputs_of(&world).await;
    let turn = |is_error| {
        vec![
            assistant(vec![call("c1", "measure", json!({}))]),
            user(vec![ContentBlock::ToolResult {
                call_id: "c1".into(),
                content: "failed".into(),
                is_error,
            }]),
        ]
    };
    assert_ne!(
        assemble_hash(&inputs, &turn(true)),
        assemble_hash(&inputs, &turn(false))
    );
}
