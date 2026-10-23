//! Validation against the canonical JSON Schemas in `schemas/` (ADR-0024).
//!
//! The schemas are embedded at build time by `synth-schema-types`, so values
//! are checked against exactly the files the Rust types were generated from.
//! This catches what the types do not enforce, such as `minItems` and
//! non-finite numbers.

use std::collections::HashMap;

use jsonschema::error::ValidationErrorKind;
use jsonschema::{Draft, Registry, ValidationError, Validator};
use schema_types::SCHEMA_FILES;
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, ErrorCode};

/// Local base URI, so every `$ref` resolves from the embedded files and never
/// from the network.
const BASE_URI: &str = "file:///autosys/schemas/";

/// Compiled validators for a chosen set of schema files.
pub struct SchemaSet {
    validators: HashMap<String, Validator>,
}

impl SchemaSet {
    /// Compiles a validator for each path, given relative to `schemas/`, for
    /// example `"sdr/transition.schema.json"`. Fails if a path is not an
    /// embedded schema.
    pub fn compile(paths: &[&str]) -> Result<Self, Error> {
        let files = SCHEMA_FILES
            .iter()
            .map(|(path, text)| {
                serde_json::from_str::<Value>(text)
                    .map(|schema| ((*path).to_owned(), schema))
                    .map_err(|e| broken(path, e))
            })
            .collect::<Result<HashMap<String, Value>, Error>>()?;
        let registry = Registry::new()
            .extend(
                files
                    .iter()
                    .map(|(path, schema)| (format!("{BASE_URI}{path}"), schema.clone())),
            )
            .and_then(|r| r.prepare())
            .map_err(|e| broken("registry", e))?;
        let mut validators = HashMap::new();
        for path in paths {
            let schema = files.get(*path).ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!("{path} is not a schema in schemas/"),
                )
            })?;
            let validator = jsonschema::options()
                .with_draft(Draft::Draft202012)
                .with_base_uri(format!("{BASE_URI}{path}"))
                .with_registry(&registry)
                .should_validate_formats(true)
                .build(schema)
                .map_err(|e| broken(path, e))?;
            validators.insert((*path).to_owned(), validator);
        }
        Ok(Self { validators })
    }

    /// Every way `value` breaks the schema at `path`, as
    /// `<instance path>: <message>`; empty when it is valid. Fails if `path`
    /// was not compiled into this set.
    pub fn errors(&self, path: &str, value: &Value) -> Result<Vec<String>, Error> {
        let validator = self.validators.get(path).ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                format!("schema {path} was not compiled into this set"),
            )
        })?;
        let mut messages = Vec::new();
        for error in validator.iter_errors(value) {
            describe(&error, &mut messages);
        }
        Ok(messages)
    }

    /// Like [`Self::errors`], for a typed value in its JSON form, which is how
    /// it is stored or sent.
    pub fn errors_of(&self, path: &str, value: &impl Serialize) -> Result<Vec<String>, Error> {
        let json = serde_json::to_value(value).map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                "cannot serialize a value for validation",
            )
            .with_source(e)
        })?;
        self.errors(path, &json)
    }
}

/// One message per error. The schemas' unions are `oneOf` branches told apart
/// by a tag (`op`, `kind`, `comparator`). When exactly one branch's tag
/// matches, its errors are reported instead of "not valid under any of the
/// schemas", so the message names the real cause.
fn describe(error: &ValidationError<'_>, out: &mut Vec<String>) {
    if let ValidationErrorKind::OneOfNotValid { context } = error.kind() {
        let at = error.instance_path().to_string();
        let tagged: Vec<&Vec<ValidationError<'static>>> = context
            .iter()
            .filter(|branch| !branch.iter().any(|e| is_tag_mismatch(e, &at)))
            .collect();
        if let [branch] = tagged.as_slice() {
            for inner in branch.iter() {
                describe(inner, out);
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const AUTONOMY: &str = "config/autonomy-policy.schema.json";

    #[test]
    fn valid_values_have_no_errors() {
        let set = SchemaSet::compile(&[AUTONOMY]).unwrap();
        let policy = json!({ "schema_version": "0.1", "approvals": [] });
        assert!(set.errors(AUTONOMY, &policy).unwrap().is_empty());
    }

    #[test]
    fn errors_name_the_path_and_the_matching_union_branch() {
        let set = SchemaSet::compile(&[AUTONOMY]).unwrap();
        let policy = json!({
            "schema_version": "0.1",
            "approvals": [{
                "id": "a",
                "description": "d",
                "trigger": { "kind": "sdr_commit" }
            }]
        });
        let errors = set.errors(AUTONOMY, &policy).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("/approvals/0/trigger: ") && errors[0].contains("final_only"),
            "{errors:?}"
        );
    }

    #[test]
    fn unknown_or_uncompiled_schemas_are_errors() {
        assert_eq!(
            SchemaSet::compile(&["nope.schema.json"])
                .map(|_| ())
                .unwrap_err()
                .code(),
            ErrorCode::InvalidInput
        );
        let set = SchemaSet::compile(&[]).unwrap();
        assert_eq!(
            set.errors(AUTONOMY, &json!({})).unwrap_err().code(),
            ErrorCode::Internal
        );
    }
}
