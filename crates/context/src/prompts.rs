//! Prompts from `prompts/` (blueprint section 12, ADR-0029).
//!
//! A prompt file is a YAML header between `---` lines, then the prompt text.
//! The header is checked against `schemas/prompts/prompt-header.schema.json`.
//! A prompt's provenance is its id, version and the sha256 of the normalised
//! file, so a run records exactly which wording it used.
//!
//! Prompts are read once, at startup, with `std::fs`, the way `infra` loads
//! `config/`. There are no template variables in v0: everything dynamic goes
//! into the sections of the context, never into a prompt.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use contracts::SchemaSet;
use contracts::types::{PromptHeader, SCHEMA_FILES, VersionedHash};
use serde_json::Value;
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy, Options};
use sha2::{Digest, Sha256};

use crate::error::ContextError;

const HEADER_SCHEMA: &str = "prompts/prompt-header.schema.json";

/// One prompt: its validated header and its text.
#[derive(Clone, Debug)]
pub struct Prompt {
    path: String,
    header: PromptHeader,
    body: String,
    provenance: VersionedHash,
}

impl Prompt {
    /// The path relative to `prompts/`, for example `system/base.md`.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn header(&self) -> &PromptHeader {
        &self.header
    }

    /// The text after the header, normalised.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Id, version and hash of the normalised file, for run provenance.
    pub fn provenance(&self) -> &VersionedHash {
        &self.provenance
    }

    /// For example `prompt:system.base@0.1.0`.
    pub(crate) fn label(&self) -> String {
        format!(
            "prompt:{}@{}",
            self.header.id.as_str(),
            self.header.version.as_str()
        )
    }
}

/// Every prompt, loaded and checked. Paths are relative to `prompts/`.
#[derive(Clone, Debug, Default)]
pub struct PromptLibrary {
    prompts: BTreeMap<String, Prompt>,
}

impl PromptLibrary {
    /// Loads every `*.md` file below a subfolder of `dir`. Files directly in
    /// `dir`, such as the README, are documentation and are skipped, as is any
    /// file that is not Markdown.
    pub fn load_dir(dir: &Path) -> Result<Self, ContextError> {
        let mut files = Vec::new();
        for sub in sorted_entries(dir, dir)? {
            if sub.is_dir() {
                collect_markdown(dir, &sub, &mut files)?;
            }
        }
        Self::from_files(files)
    }

    /// Builds a library from `(path, file text)` pairs, as `load_dir` reads them.
    ///
    /// Fails on the first prompt that is invalid: no header, a header that
    /// breaks the schema (an unknown section in `inputs`, a malformed
    /// version), an `output_schema` that names no schema, no text, or an id
    /// another prompt already has.
    pub fn from_files(files: Vec<(String, String)>) -> Result<Self, ContextError> {
        let schemas = SchemaSet::compile(&[HEADER_SCHEMA]).map_err(|e| {
            ContextError::Internal(format!("cannot compile the prompt header schema: {e}"))
        })?;
        let mut prompts: BTreeMap<String, Prompt> = BTreeMap::new();
        let mut ids: BTreeMap<String, String> = BTreeMap::new();
        for (path, text) in files {
            let prompt = parse(&schemas, &path, &text)?;
            if let Some(other) = ids.insert(prompt.header.id.as_str().to_owned(), path.clone()) {
                return Err(invalid(
                    &path,
                    vec![format!(
                        "the id `{}` is already used by `{other}`",
                        prompt.header.id.as_str()
                    )],
                ));
            }
            prompts.insert(path, prompt);
        }
        Ok(Self { prompts })
    }

    pub fn get(&self, path: &str) -> Result<&Prompt, ContextError> {
        self.prompts
            .get(path)
            .ok_or_else(|| ContextError::UnknownPrompt {
                path: path.to_owned(),
                available: self.paths().map(str::to_owned).collect(),
            })
    }

    /// Prompt paths in order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.prompts.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.prompts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }
}

fn invalid(path: &str, problems: Vec<String>) -> ContextError {
    ContextError::InvalidPrompt {
        path: path.to_owned(),
        problems,
    }
}

/// A byte-order mark and CRLF line endings must not change a prompt's hash,
/// whatever the checkout did to the file.
fn normalise(text: &str) -> String {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// `(header YAML, text)`, if the file starts with a header.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches('\n') == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// A path an agent's `prompt` field may name: `^[a-z0-9_-]+(/[a-z0-9_-]+)*\.md$`.
fn is_valid_path(path: &str) -> bool {
    let Some(stem) = path.strip_suffix(".md") else {
        return false;
    };
    stem.split('/').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    })
}

fn parse(schemas: &SchemaSet, path: &str, raw: &str) -> Result<Prompt, ContextError> {
    if !is_valid_path(path) {
        return Err(invalid(
            path,
            vec!["the path must be lowercase letters, digits, `_` and `-`, in folders, ending in .md".into()],
        ));
    }
    let text = normalise(raw);
    let Some((yaml, body)) = split_front_matter(&text) else {
        return Err(invalid(
            path,
            vec!["the file must start with a YAML header between `---` lines".into()],
        ));
    };
    let value = yaml_value(yaml).map_err(|problem| invalid(path, vec![problem]))?;
    let mut problems = schemas
        .errors(HEADER_SCHEMA, &value)
        .map_err(|e| ContextError::Internal(e.message().to_owned()))?;
    if !problems.is_empty() {
        return Err(invalid(path, problems));
    }
    let header: PromptHeader = serde_json::from_value(value).map_err(|e| {
        invalid(
            path,
            vec![format!("the header does not match its type: {e}")],
        )
    })?;

    if let Some(schema) = &header.output_schema
        && !SCHEMA_FILES.iter().any(|(p, _)| *p == schema.as_str())
    {
        problems.push(format!(
            "output_schema {} is not a schema in schemas/",
            schema.as_str()
        ));
    }
    let body = body.trim_matches('\n').to_owned();
    if body.is_empty() {
        problems.push("the prompt has no text after its header".into());
    }
    if !problems.is_empty() {
        return Err(invalid(path, problems));
    }

    let digest: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let sha256 = format!("sha256:{digest}").parse().map_err(|_| {
        ContextError::Internal("a computed digest is not a valid Sha256".to_owned())
    })?;
    let provenance = VersionedHash {
        id: header.id.clone(),
        version: Some(header.version.clone()),
        sha256,
    };
    Ok(Prompt {
        path: path.to_owned(),
        header,
        body,
        provenance,
    })
}

/// The same YAML rules as `config/`: YAML 1.2 booleans, and duplicate keys,
/// merge keys and custom tags are errors.
fn yaml_value(yaml: &str) -> Result<Value, String> {
    // Options is non-exhaustive: start from the defaults, which keep the
    // parsing budget on.
    let mut options = Options::default();
    options.strict_booleans = true;
    options.duplicate_keys = DuplicateKeyPolicy::Error;
    options.merge_keys = MergeKeyPolicy::Error;
    options.reject_unsupported_tags = true;
    serde_saphyr::from_str_with_options(yaml, options)
        .map_err(|e| format!("the header is not valid YAML: {e}"))
}

fn sorted_entries(root: &Path, dir: &Path) -> Result<Vec<std::path::PathBuf>, ContextError> {
    let unreadable = |what: &Path, e: std::io::Error| {
        invalid(
            &what.display().to_string(),
            vec![format!("cannot read: {e}")],
        )
    };
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| unreadable(dir, e))? {
        paths.push(entry.map_err(|e| unreadable(root, e))?.path());
    }
    paths.sort();
    Ok(paths)
}

fn collect_markdown(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), ContextError> {
    for path in sorted_entries(root, dir)? {
        if path.is_dir() {
            collect_markdown(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = fs::read_to_string(&path)
                .map_err(|e| invalid(&relative, vec![format!("cannot read: {e}")]))?;
            out.push((relative, text));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\n\
        schema_version: \"0.1\"\n\
        id: test.prompt\n\
        version: 1.2.3\n\
        purpose: A prompt for tests.\n\
        inputs:\n  - task_contract\n  - design_state\n\
        owner: harness\n\
        origin: human\n\
        ---\n\
        \n\
        Do the work.\n";

    fn lib(files: &[(&str, &str)]) -> Result<PromptLibrary, ContextError> {
        PromptLibrary::from_files(
            files
                .iter()
                .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
                .collect(),
        )
    }

    fn problems(result: Result<PromptLibrary, ContextError>) -> String {
        match result {
            Err(ContextError::InvalidPrompt { problems, .. }) => problems.join("; "),
            other => panic!("expected InvalidPrompt, got {:?}", other.map(|l| l.len())),
        }
    }

    #[test]
    fn a_valid_prompt_loads_with_its_provenance() {
        let library = lib(&[("agents/test.md", GOOD)]).expect("loads");
        let prompt = library.get("agents/test.md").expect("present");
        assert_eq!(prompt.body(), "Do the work.");
        assert_eq!(prompt.header().id.as_str(), "test.prompt");
        assert_eq!(prompt.label(), "prompt:test.prompt@1.2.3");
        let provenance = prompt.provenance();
        assert_eq!(provenance.id.as_str(), "test.prompt");
        assert_eq!(
            provenance.version.as_ref().map(|v| v.as_str()),
            Some("1.2.3")
        );
        assert!(provenance.sha256.as_str().starts_with("sha256:"));
    }

    #[test]
    fn crlf_and_a_byte_order_mark_do_not_change_the_hash() {
        let plain = lib(&[("a/p.md", GOOD)]).expect("loads");
        let crlf = lib(&[("a/p.md", &GOOD.replace('\n', "\r\n"))]).expect("loads");
        let bom = lib(&[("a/p.md", &format!("\u{feff}{GOOD}"))]).expect("loads");
        let hash = |l: &PromptLibrary| {
            l.get("a/p.md")
                .expect("present")
                .provenance()
                .sha256
                .clone()
        };
        assert_eq!(hash(&plain), hash(&crlf));
        assert_eq!(hash(&plain), hash(&bom));
        assert_eq!(crlf.get("a/p.md").expect("present").body(), "Do the work.");
    }

    #[test]
    fn one_changed_byte_changes_the_hash() {
        let a = lib(&[("a/p.md", GOOD)]).expect("loads");
        let b = lib(&[("a/p.md", &GOOD.replace("Do the work.", "Do the work!"))]).expect("loads");
        let hash = |l: &PromptLibrary| {
            l.get("a/p.md")
                .expect("present")
                .provenance()
                .sha256
                .clone()
        };
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn a_missing_header_is_rejected() {
        assert!(
            problems(lib(&[("a/p.md", "Just text.\n")])).contains("must start with a YAML header")
        );
        // An unterminated header is no header.
        assert!(
            problems(lib(&[("a/p.md", "---\nid: x\nno end")]))
                .contains("must start with a YAML header")
        );
    }

    #[test]
    fn header_problems_name_the_field() {
        let bad_version = GOOD.replace("version: 1.2.3", "version: \"1.2\"");
        let message = problems(lib(&[("a/p.md", &bad_version)]));
        assert!(message.contains("/version"), "{message}");

        let unknown_input = GOOD.replace("  - design_state", "  - allowed_capabilities");
        let message = problems(lib(&[("a/p.md", &unknown_input)]));
        assert!(message.contains("allowed_capabilities"), "{message}");
        assert!(message.contains("/inputs/1"), "{message}");

        let missing_owner = GOOD.replace("owner: harness\n", "");
        assert!(problems(lib(&[("a/p.md", &missing_owner)])).contains("owner"));

        let extra = GOOD.replace("origin: human", "origin: human\nextra: 1");
        assert!(problems(lib(&[("a/p.md", &extra)])).contains("extra"));

        let duplicate_key = GOOD.replace("origin: human", "origin: human\nowner: other");
        assert!(problems(lib(&[("a/p.md", &duplicate_key)])).contains("not valid YAML"));
    }

    #[test]
    fn an_output_schema_must_name_a_schema() {
        let known = GOOD.replace(
            "origin: human",
            "origin: human\noutput_schema: results/result.schema.json",
        );
        lib(&[("a/p.md", &known)]).expect("a real schema");
        let typo = GOOD.replace(
            "origin: human",
            "origin: human\noutput_schema: results/reslt.schema.json",
        );
        let message = problems(lib(&[("a/p.md", &typo)]));
        assert!(message.contains("results/reslt.schema.json"), "{message}");
    }

    #[test]
    fn a_prompt_needs_text_a_valid_path_and_a_unique_id() {
        let no_text = "---\nschema_version: \"0.1\"\nid: x\nversion: 1.0.0\npurpose: p\ninputs: []\nowner: o\norigin: human\n---\n\n";
        assert!(problems(lib(&[("a/p.md", no_text)])).contains("no text"));

        assert!(problems(lib(&[("A/P.md", GOOD)])).contains("lowercase"));
        assert!(problems(lib(&[("a/p.txt", GOOD)])).contains("ending in .md"));

        let message = problems(lib(&[("a/one.md", GOOD), ("a/two.md", GOOD)]));
        assert!(
            message.contains("`test.prompt` is already used by `a/one.md`"),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_prompt_lists_the_available_ones() {
        let library = lib(&[("a/p.md", GOOD)]).expect("loads");
        match library.get("a/other.md") {
            Err(ContextError::UnknownPrompt { path, available }) => {
                assert_eq!(path, "a/other.md");
                assert_eq!(available, ["a/p.md"]);
            }
            other => panic!("expected UnknownPrompt, got {:?}", other.map(|p| p.path())),
        }
    }

    #[test]
    fn load_dir_reads_subfolders_and_skips_documentation() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("agents/nested")).expect("mkdir");
        fs::write(dir.path().join("README.md"), "docs, no header").expect("write");
        fs::write(dir.path().join("agents/a.md"), GOOD).expect("write");
        fs::write(
            dir.path().join("agents/nested/b.md"),
            GOOD.replace("test.prompt", "test.nested"),
        )
        .expect("write");
        fs::write(dir.path().join("agents/notes.txt"), "not markdown").expect("write");

        let library = PromptLibrary::load_dir(dir.path()).expect("loads");
        let paths: Vec<&str> = library.paths().collect();
        assert_eq!(paths, ["agents/a.md", "agents/nested/b.md"]);
    }

    #[test]
    fn a_missing_directory_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("nope");
        match PromptLibrary::load_dir(&missing) {
            Err(ContextError::InvalidPrompt { path, problems }) => {
                assert!(path.ends_with("nope"), "{path}");
                assert!(problems[0].contains("cannot read"), "{problems:?}");
            }
            other => panic!("expected InvalidPrompt, got {:?}", other.map(|l| l.len())),
        }
    }
}
