//! Turns what a tool returned into what the harness keeps: a result checked
//! against its canonical schema, and the quantities in it as evidence.

use contracts::ids::{EvidenceId, Identifier};
use contracts::types::{NamedQuantity, Quantity, Sha256};
use contracts::{Error, ErrorCode, SchemaSet, ToolOutput, ToolRequest};
use serde_json::{Map, Value};
use sha2::Digest;

/// The structured result, if it is an object that satisfies `schema`. Tool
/// output is untrusted, so anything else is a tool failure.
pub(crate) fn validated_result(
    output: ToolOutput,
    schemas: &SchemaSet,
    schema: &str,
    tool: &str,
) -> Result<Map<String, Value>, Error> {
    let problems = schemas.errors(schema, &output.structured)?;
    if !problems.is_empty() {
        return Err(Error::new(
            ErrorCode::ToolFailed,
            format!(
                "{tool} returned a result that breaks {schema}: {}",
                problems.join("; ")
            ),
        ));
    }
    match output.structured {
        Value::Object(result) => Ok(result),
        _ => Err(Error::new(
            ErrorCode::ToolFailed,
            format!("{tool} returned a result that is not an object"),
        )),
    }
}

/// Every `{value, unit}` object in `result`, named by its path, for example
/// `mass` or `bounding_box.length`. Values that are not finite numbers or
/// units that are not valid codes are left out.
pub(crate) fn quantities(result: &Map<String, Value>) -> Vec<NamedQuantity> {
    let mut found = Vec::new();
    for (key, value) in result {
        collect(key, value, &mut found);
    }
    found
}

fn collect(path: &str, value: &Value, out: &mut Vec<NamedQuantity>) {
    match value {
        Value::Object(map) => {
            if let Some(quantity) = as_quantity(map) {
                if let Ok(name) = path.parse::<Identifier>() {
                    out.push(NamedQuantity { name, quantity });
                }
                return;
            }
            for (key, inner) in map {
                collect(&format!("{path}.{key}"), inner, out);
            }
        }
        Value::Array(items) => {
            for (i, inner) in items.iter().enumerate() {
                collect(&format!("{path}.{i}"), inner, out);
            }
        }
        _ => {}
    }
}

fn as_quantity(map: &Map<String, Value>) -> Option<Quantity> {
    if map.len() != 2 {
        return None;
    }
    let value = map.get("value")?.as_f64().filter(|v| v.is_finite())?;
    let unit = map.get("unit")?.as_str()?.parse().ok()?;
    Some(Quantity { value, unit })
}

/// sha256 of the canonical JSON (keys sorted) of the tool name and
/// arguments. Equal requests always hash equally.
pub(crate) fn request_sha256(request: &ToolRequest) -> Result<Sha256, Error> {
    let mut request_json = Map::new();
    request_json.insert(
        "arguments".to_owned(),
        Value::Object(request.arguments.clone()),
    );
    request_json.insert(
        "tool".to_owned(),
        Value::String(request.tool.as_str().to_owned()),
    );
    let canonical = canonical(&Value::Object(request_json));
    format!(
        "sha256:{}",
        hex(&sha2::Sha256::digest(canonical.to_string()))
    )
    .parse()
    .map_err(|_| Error::new(ErrorCode::Internal, "request hash is not a valid sha256"))
}

/// The same value with every object's keys in sorted order, whichever map
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

/// The evidence id of a step: derived from its idempotency key, so a step
/// re-run after an interruption writes the same evidence again.
pub(crate) fn evidence_id(key: &Identifier) -> Result<EvidenceId, Error> {
    let digest = sha2::Sha256::digest(key.as_str());
    format!("ev-tool-{}", hex(&digest[..16]))
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "generated evidence id is invalid"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn quantities_are_named_by_their_path() {
        let result = object(json!({
            "part_id": "sha256:00",
            "mass": { "value": 122.5, "unit": "g" },
            "bounding_box": {
                "length": { "value": 110, "unit": "mm" },
                "width": { "value": 70, "unit": "mm" }
            },
            "holes": [{ "value": 6.5, "unit": "mm" }],
            "not_a_quantity": { "value": 1, "unit": "mm", "extra": true },
            "bad_unit": { "value": 1, "unit": "" }
        }));
        let names: Vec<String> = quantities(&result)
            .iter()
            .map(|q| {
                format!(
                    "{}={}{}",
                    q.name.as_str(),
                    q.quantity.value,
                    q.quantity.unit.as_str()
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                "bounding_box.length=110mm",
                "bounding_box.width=70mm",
                "holes.0=6.5mm",
                "mass=122.5g",
            ]
        );
    }

    #[test]
    fn request_hashes_ignore_key_order_and_see_every_value() {
        let request = |arguments: Value| ToolRequest {
            tool: "create_plate".parse().unwrap(),
            step_id: "step-1".parse().unwrap(),
            idempotency_key: "run-1:step-1".parse().unwrap(),
            arguments: object(arguments),
        };
        let a = request_sha256(&request(json!({ "a": 1, "b": { "x": 1, "y": 2 } }))).unwrap();
        let b = request_sha256(&request(json!({ "b": { "y": 2, "x": 1 }, "a": 1 }))).unwrap();
        let c = request_sha256(&request(json!({ "a": 1, "b": { "x": 1, "y": 3 } }))).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn evidence_ids_are_stable_per_key() {
        let key: Identifier = "run-1:step-1".parse().unwrap();
        let other: Identifier = "run-1:step-2".parse().unwrap();
        assert_eq!(evidence_id(&key).unwrap(), evidence_id(&key).unwrap());
        assert_ne!(evidence_id(&key).unwrap(), evidence_id(&other).unwrap());
    }
}
