//! SDR document schema migrations (blueprint section 7.6). Every schema change
//! bumps `schema_version` and adds a step here, with tests.
//!
//! Version 0.1 (ADR-0020) is the first schema version, so there is nothing to
//! migrate yet. Repository reads return typed documents of the current
//! version; they will go through [`upgrade`] once the first real migration
//! exists.

use contracts::{Error, ErrorCode};
use serde_json::Value;

/// The schema version this build writes.
pub(crate) const CURRENT: &str = "0.1";

/// One step: turns a document of version `from` into version `to`.
struct Migration {
    from: &'static str,
    to: &'static str,
    apply: fn(Value) -> Result<Value, String>,
}

/// Every step, oldest first.
const MIGRATIONS: &[Migration] = &[];

/// Brings a raw document up to [`CURRENT`], one step at a time.
pub(crate) fn upgrade(mut document: Value) -> Result<Value, Error> {
    // Each step must move forward, so the chain ends within its length.
    for _ in 0..=MIGRATIONS.len() {
        let version = document
            .get("schema_version")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("the document has no schema_version".to_owned()))?
            .to_owned();
        if version == CURRENT {
            return Ok(document);
        }
        let step = MIGRATIONS
            .iter()
            .find(|m| m.from == version)
            .ok_or_else(|| invalid(format!("unknown SDR schema version {version}")))?;
        document = (step.apply)(document)
            .map_err(|e| invalid(format!("migrating from {} to {}: {e}", step.from, step.to)))?;
        document["schema_version"] = Value::from(step.to);
    }
    Err(Error::new(
        ErrorCode::Internal,
        "SDR migrations do not reach the current schema version",
    ))
}

fn invalid(message: String) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_current_document_is_unchanged() {
        let doc = json!({ "schema_version": CURRENT, "anything": [1, 2] });
        assert_eq!(upgrade(doc.clone()).unwrap(), doc);
    }

    #[test]
    fn unknown_or_missing_versions_are_rejected() {
        for doc in [
            json!({ "schema_version": "9.9" }),
            json!({}),
            json!({ "schema_version": 1 }),
        ] {
            assert_eq!(upgrade(doc).unwrap_err().code(), ErrorCode::InvalidInput);
        }
    }

    #[test]
    fn every_step_moves_to_a_known_version() {
        for step in MIGRATIONS {
            assert_ne!(step.from, step.to);
            assert!(
                step.to == CURRENT || MIGRATIONS.iter().any(|m| m.from == step.to),
                "{} leads nowhere",
                step.to
            );
        }
    }
}
