//! The prompts in `prompts/` (blueprint section 12): headers are valid, the
//! text is static, and the instructions fit their budget.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use context::{ByteEstimate, ContextError, PromptLibrary, TokenCounter};
use contracts::types::{ContextSectionName, PromptHeaderOrigin};
use support::*;

#[test]
fn the_repository_prompts_load_with_valid_headers() {
    let library = prompts();
    let paths: Vec<&str> = library.paths().collect();
    assert_eq!(paths, ["orchestrator/main.md", "system/base.md"]);

    let base = library.get("system/base.md").expect("base");
    assert_eq!(base.header().id.as_str(), "system.base");
    let main = library.get("orchestrator/main.md").expect("orchestrator");
    assert_eq!(main.header().id.as_str(), "orchestrator.main");
    for prompt in [base, main] {
        let header = prompt.header();
        assert_eq!(header.version.as_str(), "0.1.0");
        assert_eq!(header.owner.as_str(), "harness");
        assert_eq!(header.origin, PromptHeaderOrigin::Human);
        assert!(!header.inputs.is_empty());
        assert!(!prompt.body().is_empty());
        assert!(prompt.provenance().sha256.as_str().starts_with("sha256:"));
        assert_eq!(prompt.provenance().id, header.id);
    }
    // The base prompt is for every context, so it lists every section.
    assert_eq!(base.header().inputs.len(), 7);
    assert!(
        base.header()
            .inputs
            .contains(&ContextSectionName::Instructions)
    );
}

#[test]
fn the_readme_is_documentation_not_a_prompt() {
    let library = prompts();
    assert!(
        library
            .paths()
            .all(|p| !p.to_lowercase().contains("readme"))
    );
    assert!(repo_root().join("prompts/README.md").exists());
}

#[test]
fn prompts_are_static_text() {
    // No template variables or includes in v0: dynamic content goes into
    // the sections of the context, never into a prompt.
    let library = prompts();
    for path in library.paths() {
        let body = library.get(path).expect("prompt").body();
        for marker in ["{{", "}}", "${", "{%", "<%"] {
            assert!(!body.contains(marker), "{path} contains `{marker}`");
        }
        assert!(!body.contains('\r'), "{path} was not normalised");
    }
}

#[test]
fn a_prompt_names_no_tool() {
    // Which tools exist is in the task contract (blueprint section 12).
    let library = prompts();
    for path in library.paths() {
        let body = library.get(path).expect("prompt").body();
        for tool in ["sdr_read", "sdr_propose", "create_plate", "measure"] {
            assert!(!body.contains(tool), "{path} names the tool {tool}");
        }
    }
}

#[test]
fn the_instructions_fit_the_default_profile_with_room_to_spare() {
    let library = prompts();
    let profile = default_profile();
    let counter = ByteEstimate::for_profile(&profile);
    let system = ["system/base.md", "orchestrator/main.md"]
        .iter()
        .map(|p| library.get(p).expect("prompt").body())
        .collect::<Vec<_>>()
        .join("\n\n");
    let tokens = counter.count(&system);
    let cap = profile.sections.instructions.get();
    assert!(
        tokens * 2 <= cap,
        "the instructions use {tokens} of {cap} tokens"
    );
}

#[test]
fn a_header_naming_a_section_that_does_not_exist_is_rejected_and_named() {
    let text = "---\nschema_version: \"0.1\"\nid: agents.bad\nversion: 0.1.0\npurpose: A prompt.\n\
                inputs:\n  - task_contract\n  - allowed_capabilities\nowner: harness\norigin: human\n---\n\nText.\n";
    match PromptLibrary::from_files(vec![("agents/bad.md".into(), text.into())]) {
        Err(ContextError::InvalidPrompt { path, problems }) => {
            assert_eq!(path, "agents/bad.md");
            let all = problems.join("; ");
            assert!(all.contains("allowed_capabilities"), "{all}");
            assert!(all.contains("/inputs/1"), "{all}");
        }
        other => panic!("expected InvalidPrompt, got {:?}", other.map(|l| l.len())),
    }
}

#[test]
fn the_example_header_in_the_schemas_is_a_valid_prompt_header() {
    // The example the schema tests validate is also what a real header looks like.
    let example =
        std::fs::read_to_string(repo_root().join("schemas/prompts/examples/prompt-header.json"))
            .expect("example");
    let value: serde_json::Value = serde_json::from_str(&example).expect("json");
    let mut yaml = String::from("---\n");
    for (key, field) in value.as_object().expect("object") {
        yaml.push_str(&format!("{key}: {field}\n"));
    }
    yaml.push_str("---\n\nText.\n");
    PromptLibrary::from_files(vec![("orchestrator/example.md".into(), yaml)])
        .expect("the example header is accepted");
}

#[test]
fn an_unreadable_directory_is_reported_with_its_path() {
    match PromptLibrary::load_dir(&repo_root().join("prompts-that-do-not-exist")) {
        Err(ContextError::InvalidPrompt { path, .. }) => {
            assert!(path.contains("prompts-that-do-not-exist"), "{path}");
        }
        other => panic!("expected InvalidPrompt, got {:?}", other.map(|l| l.len())),
    }
}

fn prompt_file(id: &str, version: &str, body: &str) -> String {
    format!(
        "---\nschema_version: \"0.1\"\nid: {id}\nversion: {version}\npurpose: A prompt.\n\
         inputs:\n  - task_contract\nowner: harness\norigin: human\n---\n\n{body}\n"
    )
}

fn problems_of(files: Vec<(&str, String)>) -> String {
    let files = files.into_iter().map(|(p, t)| (p.to_owned(), t)).collect();
    match PromptLibrary::from_files(files) {
        Err(ContextError::InvalidPrompt { problems, .. }) => problems.join("; "),
        other => panic!("expected InvalidPrompt, got {:?}", other.map(|l| l.len())),
    }
}

#[test]
fn a_prompt_without_a_header_is_rejected() {
    let message = problems_of(vec![("agents/a.md", "Just text, no header.\n".to_owned())]);
    assert!(message.contains("YAML header"), "{message}");
}

#[test]
fn a_version_that_is_not_semver_is_rejected_and_named() {
    let message = problems_of(vec![(
        "agents/a.md",
        prompt_file("agents.a", "\"1.0\"", "Text."),
    )]);
    assert!(message.contains("/version"), "{message}");
}

#[test]
fn two_prompts_with_one_id_are_rejected() {
    let message = problems_of(vec![
        ("agents/a.md", prompt_file("agents.same", "0.1.0", "One.")),
        ("agents/b.md", prompt_file("agents.same", "0.1.0", "Two.")),
    ]);
    assert!(message.contains("agents.same"), "{message}");
    assert!(message.contains("agents/a.md"), "{message}");
}

#[test]
fn crlf_and_a_byte_order_mark_give_the_same_hash() {
    let text = prompt_file("agents.a", "0.1.0", "Line one.\nLine two.");
    let hash_of = |text: String| {
        PromptLibrary::from_files(vec![("agents/a.md".to_owned(), text)])
            .expect("loads")
            .get("agents/a.md")
            .expect("present")
            .provenance()
            .sha256
            .clone()
    };
    let plain = hash_of(text.clone());
    assert_eq!(plain, hash_of(text.replace('\n', "\r\n")));
    assert_eq!(plain, hash_of(format!("\u{feff}{text}")));
    assert_ne!(plain, hash_of(text.replace("two", "2")));
}
