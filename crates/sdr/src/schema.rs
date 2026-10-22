//! Validation against the canonical JSON Schemas, embedded at build time by
//! `synth-schema-types`. This catches what the generated Rust types do not
//! enforce, such as `minItems` and non-finite numbers.

use contracts::types::{SCHEMA_FILES, SdrDocument, Transition};
use contracts::{Error, ErrorCode};
use jsonschema::error::ValidationErrorKind;
use jsonschema::{Draft, Registry, ValidationError, Validator};
use serde::Serialize;
use serde_json::Value;

/// Local base URI, so every `$ref` resolves from the embedded files and never
/// from the network.
const BASE_URI: &str = "file:///autosys/schemas/";
const TRANSITION: &str = "sdr/transition.schema.json";
const DOCUMENT: &str = "sdr/system.schema.json";

pub(crate) struct Schemas {
    transition: Validator,
    document: Validator,
}

impl Schemas {
    /// Compiles the validators. Fails only if the embedded schemas are broken,
    /// which the schema-types tests rule out.
    pub(crate) fn load() -> Result<Self, Error> {
        let files = SCHEMA_FILES
            .iter()
            .map(|(path, text)| {
                serde_json::from_str::<Value>(text)
                    .map(|schema| (format!("{BASE_URI}{path}"), schema))
                    .map_err(|e| broken(path, e))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let registry = Registry::new()
            .extend(files.iter().cloned())
            .and_then(|r| r.prepare())
            .map_err(|e| broken("the schema registry", e))?;
        let compile = |path: &str| -> Result<Validator, Error> {
            let schema = files
                .iter()
                .find(|(uri, _)| uri.ends_with(path))
                .map(|(_, schema)| schema)
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::Internal,
                        format!("schema {path} is not embedded"),
                    )
                })?;
            jsonschema::options()
                .with_draft(Draft::Draft202012)
                .with_base_uri(format!("{BASE_URI}{path}"))
                .with_registry(&registry)
                .should_validate_formats(true)
                .build(schema)
                .map_err(|e| broken(path, e))
        };
        Ok(Self {
            transition: compile(TRANSITION)?,
            document: compile(DOCUMENT)?,
        })
    }

    /// Every way `transition` breaks `transition.schema.json`.
    pub(crate) fn transition_errors(&self, transition: &Transition) -> Result<Vec<String>, Error> {
        errors(&self.transition, transition)
    }

    /// Every way `document` breaks `system.schema.json`.
    pub(crate) fn document_errors(&self, document: &SdrDocument) -> Result<Vec<String>, Error> {
        errors(&self.document, document)
    }

    /// Every way a raw document breaks `system.schema.json`.
    pub(crate) fn raw_document_errors(&self, document: &Value) -> Vec<String> {
        describe(&self.document, document)
    }
}

/// Validates the value as it would be stored or sent: its JSON form.
fn errors(validator: &Validator, value: &impl Serialize) -> Result<Vec<String>, Error> {
    let json = serde_json::to_value(value).map_err(|e| {
        Error::new(
            ErrorCode::Internal,
            "cannot serialize a value for validation",
        )
        .with_source(e)
    })?;
    Ok(describe(validator, &json))
}

fn describe(validator: &Validator, json: &Value) -> Vec<String> {
    let mut messages = Vec::new();
    for error in validator.iter_errors(json) {
        describe_error(&error, &mut messages);
    }
    messages
}

/// One message per error, as `<instance path>: <message>`. The SDR unions
/// are `oneOf` branches told apart by a tag (`op`, `kind`, `comparator`).
/// When exactly one branch's tag matches, its errors are reported instead of
/// "not valid under any of the schemas", so the message names the real cause.
fn describe_error(error: &ValidationError<'_>, out: &mut Vec<String>) {
    if let ValidationErrorKind::OneOfNotValid { context } = error.kind() {
        let at = error.instance_path().to_string();
        let tagged: Vec<&Vec<ValidationError<'static>>> = context
            .iter()
            .filter(|branch| !branch.iter().any(|e| is_tag_mismatch(e, &at)))
            .collect();
        if let [branch] = tagged.as_slice() {
            for inner in branch.iter() {
                describe_error(inner, out);
            }
            return;
        }
    }
    let path = error.instance_path().to_string();
    let at = if path.is_empty() {
        "/".to_owned()
    } else {
        path
    };
    out.push(format!("{at}: {error}"));
}

/// A `const` failure on a direct property of the union value: its tag.
fn is_tag_mismatch(error: &ValidationError<'_>, union_path: &str) -> bool {
    let path = error.instance_path().to_string();
    matches!(error.kind(), ValidationErrorKind::Constant { .. })
        && path
            .rsplit_once('/')
            .is_some_and(|(parent, _)| parent == union_path)
}

fn broken(what: &str, err: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::Internal,
        format!("embedded schema {what} cannot be used: {err}"),
    )
}
