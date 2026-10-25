//! What shaped a run, projected from its session log (ADR-0033). The log is
//! the source: each claim's `job_state_changed` names the build that ran the
//! segment, each `context_assembled` the prompts, skills and agent spec, each
//! `model_call_completed` the model. The continuation keeps the same
//! projection as a cache, built with [`observe`], and the run report folds
//! the log with [`fold`].

use schema_types::{Event, EventPayload, RunHarness, VersionedHash};

/// Nothing observed yet.
pub fn empty() -> RunHarness {
    RunHarness {
        harness_builds: Vec::new(),
        model_ids: Vec::new(),
        prompts: Vec::new(),
        skills: Vec::new(),
        agent_specs: Vec::new(),
    }
}

/// Adds what `payload` records to `harness`. Builds are kept per segment, in
/// order; everything else once, in the order first seen. Other events, and
/// events written before schema 0.2, add nothing.
pub fn observe(harness: &mut RunHarness, payload: &EventPayload) {
    match payload {
        EventPayload::JobStateChanged {
            harness_build: Some(build),
            ..
        } => harness.harness_builds.push(build.clone()),
        EventPayload::ContextAssembled {
            prompts,
            skills,
            agent_spec,
            ..
        } => {
            add_all(&mut harness.prompts, prompts);
            add_all(&mut harness.skills, skills);
            add_all(&mut harness.agent_specs, agent_spec.iter());
        }
        EventPayload::ModelCallCompleted { model_id, .. }
            if !harness.model_ids.contains(model_id) =>
        {
            harness.model_ids.push(model_id.clone());
        }
        _ => {}
    }
}

/// The projection of `events`, in log order.
pub fn fold<'a>(events: impl IntoIterator<Item = &'a Event>) -> RunHarness {
    let mut harness = empty();
    for event in events {
        observe(&mut harness, &event.payload);
    }
    harness
}

fn add_all<'a>(into: &mut Vec<VersionedHash>, items: impl IntoIterator<Item = &'a VersionedHash>) {
    for item in items {
        if !into.contains(item) {
            into.push(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn event(seq: u64, payload: Value) -> Event {
        serde_json::from_value(json!({
            "schema_version": "0.2", "run_id": "run-1", "seq": seq,
            "occurred_at": "2026-10-25T10:00:00Z", "correlation_id": "corr-1",
            "actor": { "kind": "system", "component": "test" }, "payload": payload
        }))
        .expect("an event")
    }

    fn hash(id: &str, digit: char) -> Value {
        json!({ "id": id, "sha256": format!("sha256:{}", digit.to_string().repeat(64)) })
    }

    fn context(seq: u64, prompt: Value) -> Event {
        event(
            seq,
            json!({ "type": "context_assembled", "context_hash": format!("sha256:{}", "c".repeat(64)),
                "total_tokens": 10, "sdr_version": 0, "last_event_seq": seq - 1,
                "prompts": [prompt], "agent_spec": hash("main_orchestrator", 'e') }),
        )
    }

    fn claim(seq: u64, build: &str) -> Event {
        event(
            seq,
            json!({ "type": "job_state_changed", "from": "queued", "to": { "state": "running" },
                "harness_build": build }),
        )
    }

    fn model(seq: u64, id: &str) -> Event {
        event(
            seq,
            json!({ "type": "model_call_completed", "model_profile": "default", "model_id": id,
                "usage": { "input_tokens": 1, "output_tokens": 1 } }),
        )
    }

    #[test]
    fn segments_keep_their_builds_and_the_rest_is_a_set() {
        let events = [
            claim(1, "build-a"),
            context(2, hash("orchestrator.main", 'a')),
            model(3, "m-1"),
            claim(4, "build-b"),
            context(5, hash("orchestrator.main", 'a')),
            model(6, "m-1"),
            context(7, hash("orchestrator.main", 'b')),
            model(8, "m-2"),
            claim(9, "build-b"),
        ];
        let harness = fold(&events);
        let builds: Vec<&str> = harness.harness_builds.iter().map(|b| b.as_str()).collect();
        assert_eq!(builds, ["build-a", "build-b", "build-b"], "one per segment");
        let models: Vec<&str> = harness.model_ids.iter().map(|m| m.as_str()).collect();
        assert_eq!(models, ["m-1", "m-2"]);
        assert_eq!(harness.prompts.len(), 2, "the same prompt counts once");
        assert_eq!(harness.agent_specs.len(), 1);
        assert!(harness.skills.is_empty());
    }

    #[test]
    fn events_before_schema_0_2_add_nothing_but_their_model() {
        let old = event(
            1,
            json!({ "type": "context_assembled", "context_hash": format!("sha256:{}", "c".repeat(64)),
                "total_tokens": 10, "sdr_version": 0, "last_event_seq": 0 }),
        );
        let harness = fold([&old, &model(2, "m-1")]);
        assert_eq!(harness.model_ids.len(), 1);
        assert!(harness.prompts.is_empty() && harness.harness_builds.is_empty());
    }
}
