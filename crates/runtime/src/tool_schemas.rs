//! Tool definitions for the model, built from the canonical schemas
//! (blueprint sections 7.1 and 14.2, ADR-0030).
//!
//! A tool's `input_schema` is its request schema with every `$ref` inlined
//! into one `$defs` map, under the names `schema-types` gives the same
//! definitions (a root type under its `title`, a definition under its own
//! name; both are unique across `schemas/`). Its description is the request
//! schema's `description`, so model-facing text lives in `schemas/`, never
//! in Rust.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use contracts::types::SCHEMA_FILES;
use contracts::{SchemaSet, ToolSpec};
use serde_json::{Map, Value};

use crate::error::RuntimeError;

pub(crate) const READ_REQUEST: &str = "tools/sdr/sdr_read_request.schema.json";
pub(crate) const PROPOSE_REQUEST: &str = "tools/sdr/sdr_propose_request.schema.json";

/// Every embedded schema, parsed, and validators for the SDR tools'
/// requests. The tool gateway validates the requests of every other tool.
pub(crate) struct ToolSchemas {
    files: BTreeMap<String, Value>,
    requests: SchemaSet,
}

impl ToolSchemas {
    pub(crate) fn load() -> Result<Self, RuntimeError> {
        let files = SCHEMA_FILES
            .iter()
            .map(|(path, text)| {
                serde_json::from_str::<Value>(text)
                    .map(|schema| ((*path).to_owned(), schema))
                    .map_err(|e| RuntimeError::Internal(format!("schema {path} is not JSON: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let requests = SchemaSet::compile(&[READ_REQUEST, PROPOSE_REQUEST]).map_err(|e| {
            RuntimeError::Internal(format!("the SDR tool schemas do not compile: {e}"))
        })?;
        Ok(Self { files, requests })
    }

    /// The definition of tool `name`, whose request schema is `request_schema`.
    pub(crate) fn spec(
        &self,
        name: &contracts::ids::ToolName,
        request_schema: &str,
    ) -> Result<ToolSpec, RuntimeError> {
        let mut input_schema = bundle(&self.files, request_schema)?;
        let Value::Object(root) = &mut input_schema else {
            return Err(broken(request_schema, "is not an object"));
        };
        root.remove("title");
        let description = match root.remove("description") {
            Some(Value::String(text)) if !text.trim().is_empty() => text,
            _ => return Err(broken(request_schema, "has no description for the model")),
        };
        if root.get("type").and_then(Value::as_str) != Some("object") {
            return Err(broken(request_schema, "is not an object schema"));
        }
        Ok(ToolSpec {
            name: name.clone(),
            description,
            input_schema,
        })
    }

    /// Every way `arguments` break an SDR tool's request schema; empty when
    /// they are valid.
    pub(crate) fn argument_problems(
        &self,
        request_schema: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Vec<String>, RuntimeError> {
        self.requests
            .errors(request_schema, &Value::Object(arguments.clone()))
            .map_err(|e| RuntimeError::Internal(format!("validating tool arguments: {e}")))
    }
}

fn broken(path: &str, what: &str) -> RuntimeError {
    RuntimeError::Internal(format!("request schema {path} {what}"))
}

/// What a `$ref` points at: a file's root type, or one of its `$defs`.
enum Target {
    Root,
    Def(String),
}

/// `root_file` with every `$ref` it reaches, directly or not, inlined into
/// its `$defs`. `$schema` is dropped.
fn bundle(files: &BTreeMap<String, Value>, root_file: &str) -> Result<Value, RuntimeError> {
    let mut root = files
        .get(root_file)
        .cloned()
        .ok_or_else(|| broken(root_file, "is not a schema in schemas/"))?;
    let Value::Object(root_map) = &mut root else {
        return Err(broken(root_file, "is not an object"));
    };
    root_map.remove("$schema");
    let root_defs = match root_map.remove("$defs") {
        Some(Value::Object(defs)) => defs,
        Some(_) => return Err(broken(root_file, "has a `$defs` that is not an object")),
        None => Map::new(),
    };

    let mut queue = Vec::new();
    rewrite(&mut root, root_file, root_file, files, &mut queue)?;
    let mut defs = Map::new();
    let mut origin: BTreeMap<String, String> = BTreeMap::new();
    for (name, mut schema) in root_defs {
        rewrite(&mut schema, root_file, root_file, files, &mut queue)?;
        origin.insert(name.clone(), root_file.to_owned());
        defs.insert(name, schema);
    }

    while let Some((file, target)) = queue.pop() {
        let source = files.get(&file).ok_or_else(|| {
            broken(
                root_file,
                &format!("refers to {file}, which does not exist"),
            )
        })?;
        let (name, mut schema) = match &target {
            Target::Root => {
                let title = source.get("title").and_then(Value::as_str).ok_or_else(|| {
                    broken(
                        root_file,
                        &format!("refers to {file}, which has no root type"),
                    )
                })?;
                let mut schema = source.clone();
                if let Value::Object(map) = &mut schema {
                    map.remove("$schema");
                    map.remove("$defs");
                }
                (title.to_owned(), schema)
            }
            Target::Def(name) => {
                let schema = source
                    .get("$defs")
                    .and_then(|d| d.get(name))
                    .cloned()
                    .ok_or_else(|| {
                        broken(
                            root_file,
                            &format!("refers to {file}#/$defs/{name}, which is missing"),
                        )
                    })?;
                (name.clone(), schema)
            }
        };
        match origin.get(&name) {
            Some(previous) if *previous == file => continue,
            Some(previous) => {
                return Err(broken(
                    root_file,
                    &format!("reaches `{name}` in both {previous} and {file}"),
                ));
            }
            None => {}
        }
        rewrite(&mut schema, &file, root_file, files, &mut queue)?;
        origin.insert(name.clone(), file);
        defs.insert(name, schema);
    }

    if !defs.is_empty()
        && let Value::Object(map) = &mut root
    {
        map.insert("$defs".to_owned(), Value::Object(defs));
    }
    Ok(root)
}

/// Rewrites the `$ref`s in `value`, which comes from `file`, to point into
/// the bundled `$defs`, and queues what they point at.
fn rewrite(
    value: &mut Value,
    file: &str,
    root_file: &str,
    files: &BTreeMap<String, Value>,
    queue: &mut Vec<(String, Target)>,
) -> Result<(), RuntimeError> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref") {
                let (file_part, fragment) = reference
                    .split_once('#')
                    .unwrap_or((reference.as_str(), ""));
                let target_file = if file_part.is_empty() {
                    file.to_owned()
                } else {
                    resolve_relative(file, file_part)
                };
                let rewritten = if let Some(name) = fragment.strip_prefix("/$defs/") {
                    if name.contains('/') {
                        return Err(broken(file, &format!("has a nested `$ref` {reference}")));
                    }
                    if target_file != root_file {
                        queue.push((target_file, Target::Def(name.to_owned())));
                    }
                    format!("#/$defs/{name}")
                } else if fragment.is_empty() {
                    if target_file == root_file {
                        "#".to_owned()
                    } else {
                        let title = files
                            .get(&target_file)
                            .and_then(|s| s.get("title"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                broken(file, &format!("has a `$ref` {reference} to no root type"))
                            })?
                            .to_owned();
                        queue.push((target_file, Target::Root));
                        format!("#/$defs/{title}")
                    }
                } else {
                    return Err(broken(
                        file,
                        &format!("has an unsupported `$ref` {reference}"),
                    ));
                };
                *reference = rewritten;
            }
            for child in map.values_mut() {
                rewrite(child, file, root_file, files, queue)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite(item, file, root_file, files, queue)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolves `relative` against the directory of `current_file`, both
/// relative to `schemas/`.
fn resolve_relative(current_file: &str, relative: &str) -> String {
    let mut parts: Vec<String> = Path::new(current_file)
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schemas() -> ToolSchemas {
        ToolSchemas::load().expect("the embedded schemas parse")
    }

    /// Every `$ref` in `value`.
    fn refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(r)) = map.get("$ref") {
                    out.push(r.clone());
                }
                map.values().for_each(|v| refs(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }

    #[test]
    fn a_definition_is_self_contained_and_named_as_schema_types_names_it() {
        let spec = schemas()
            .spec(&"sdr_propose".parse().expect("name"), PROPOSE_REQUEST)
            .expect("spec");
        assert!(
            spec.description.starts_with("Proposes one change"),
            "{}",
            spec.description
        );
        let schema = &spec.input_schema;
        for dropped in ["$schema", "title", "description"] {
            assert!(schema.get(dropped).is_none(), "{dropped} is kept");
        }
        let defs = schema["$defs"].as_object().expect("$defs");
        for name in [
            "Operation",
            "Requirement",
            "Criterion",
            "Parameter",
            "ElementId",
        ] {
            assert!(defs.contains_key(name), "{name} is missing");
        }
        let mut found = Vec::new();
        refs(schema, &mut found);
        for reference in &found {
            let name = reference
                .strip_prefix("#/$defs/")
                .unwrap_or_else(|| panic!("{reference} is not local"));
            assert!(defs.contains_key(name), "{reference} points at nothing");
        }
    }

    #[test]
    fn the_bundled_definition_still_validates_like_the_canonical_schema() {
        let spec = schemas()
            .spec(&"sdr_propose".parse().expect("name"), PROPOSE_REQUEST)
            .expect("spec");
        let validator = jsonschema::validator_for(&spec.input_schema).expect("compiles alone");
        let request = json!({
            "reason": "Record the mass.",
            "final": false,
            "operations": [{
                "op": "set_parameter",
                "parameter": {
                    "id": "part.mass",
                    "value": { "value": 0.2, "unit": "kg" },
                    "source": { "kind": "user" },
                    "evidence_ids": []
                }
            }]
        });
        assert!(validator.is_valid(&request));
        let mut harness_fields = request.clone();
        harness_fields["base_version"] = json!(3);
        assert!(!validator.is_valid(&harness_fields));
        assert!(!validator.is_valid(&json!({ "reason": "x", "final": true, "operations": [] })));
    }

    #[test]
    fn the_build123d_definitions_bundle_across_files() {
        let spec = schemas()
            .spec(
                &"measure".parse().expect("name"),
                "tools/build123d/measure_request.schema.json",
            )
            .expect("spec");
        let defs = spec.input_schema["$defs"].as_object().expect("$defs");
        assert!(defs.contains_key("Build123dPart"));
        assert!(defs.contains_key("Build123dCreatePlateRequest"));
        assert!(defs.contains_key("Build123dDensity"));
    }

    #[test]
    fn sdr_arguments_are_checked_against_their_request_schema() {
        let schemas = schemas();
        let ok = json!({ "ids": ["R-mass"] });
        assert!(
            schemas
                .argument_problems(READ_REQUEST, ok.as_object().expect("object"))
                .expect("validates")
                .is_empty()
        );
        let bad = json!({ "version": -1 });
        let problems = schemas
            .argument_problems(READ_REQUEST, bad.as_object().expect("object"))
            .expect("validates");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("/version"), "{problems:?}");
    }

    #[test]
    fn only_an_object_schema_that_exists_can_be_a_tool_definition() {
        let name = "x".parse().expect("name");
        let union = schemas()
            .spec(&name, "tools/tool-result.schema.json")
            .map(|_| ())
            .expect_err("a oneOf root is not an object schema");
        assert!(
            union.to_string().contains("not an object schema"),
            "{union}"
        );
        let missing = schemas()
            .spec(&name, "tools/missing.schema.json")
            .map(|_| ())
            .expect_err("no such schema");
        assert!(missing.to_string().contains("not a schema"), "{missing}");
    }

    #[test]
    fn relative_references_resolve_from_the_referring_file() {
        assert_eq!(
            resolve_relative("tools/sdr/a.schema.json", "../../common/types.schema.json"),
            "common/types.schema.json"
        );
        assert_eq!(
            resolve_relative("sdr/transition.schema.json", "design.schema.json"),
            "sdr/design.schema.json"
        );
    }
}
