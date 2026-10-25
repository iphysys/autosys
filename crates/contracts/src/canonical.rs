//! The sha256 of a JSON value written canonically: object keys sorted at
//! every depth, no whitespace. A value read back from storage that reorders
//! keys, such as Postgres jsonb, hashes the same.

use schema_types::Sha256;
use serde_json::Value;
use sha2::Digest;

use crate::error::{Error, ErrorCode};

/// `sha256:` and the hex digest of `value`'s canonical JSON.
pub fn canonical_sha256(value: &Value) -> Result<Sha256, Error> {
    let digest = sha2::Sha256::digest(canonical(value).to_string());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}").parse().map_err(|_| {
        Error::new(
            ErrorCode::Internal,
            "a canonical hash is not a valid sha256",
        )
    })
}

/// The same value with every object's keys in sorted order, whatever map
/// ordering serde_json was built with.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn key_order_does_not_change_the_hash_and_values_do() {
        let a = json!({ "b": { "y": 2, "x": [1, { "q": 1, "p": 2 }] }, "a": 1 });
        let b = json!({ "a": 1, "b": { "x": [1, { "p": 2, "q": 1 }], "y": 2 } });
        let hash = canonical_sha256(&a).expect("hash");
        assert_eq!(hash, canonical_sha256(&b).expect("hash"));
        assert!(hash.as_str().starts_with("sha256:"));
        let array_order = json!({ "a": 1, "b": { "x": [{ "p": 2, "q": 1 }, 1], "y": 2 } });
        assert_ne!(hash, canonical_sha256(&array_order).expect("hash"));
        assert_ne!(hash, canonical_sha256(&json!({ "a": 2 })).expect("hash"));
    }
}
