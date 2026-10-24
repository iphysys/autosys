//! The context hash: sha256 over exactly what the model sees (ADR-0029).
//!
//! The preimage is `synth-context/v1\n`, then a length-prefixed framing of the
//! system prompt and every block of every message. Lengths are 8 bytes,
//! big-endian, so no content can be mistaken for a boundary. Maps inside
//! blocks are written as canonical JSON: keys sorted, no whitespace.
//!
//! Not hashed: the model id, the tool definitions, the output limit, the
//! manifest, the profile name and every timestamp. A change to the framing
//! must change the `v1` prefix.

use contracts::types::Sha256 as ContextHash;
use contracts::{ContentBlock, Message, Role};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::ContextError;

const PREFIX: &[u8] = b"synth-context/v1\n";

pub(crate) fn context_hash(
    system: &str,
    messages: &[Message],
) -> Result<ContextHash, ContextError> {
    let mut hasher = Sha256::new();
    hasher.update(PREFIX);
    hasher.update(b"S");
    write_bytes(&mut hasher, system.as_bytes());
    for message in messages {
        hasher.update(b"M");
        hasher.update(match message.role {
            Role::User => b"U",
            Role::Assistant => b"A",
        });
        hasher.update(length(message.content.len()));
        for block in &message.content {
            write_block(&mut hasher, block);
        }
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
        .parse()
        .map_err(|_| ContextError::Internal("a computed digest is not a valid Sha256".to_owned()))
}

fn length(len: usize) -> [u8; 8] {
    u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes()
}

fn write_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(length(bytes.len()));
    hasher.update(bytes);
}

fn write_block(hasher: &mut Sha256, block: &ContentBlock) {
    match block {
        ContentBlock::Text { text } => {
            hasher.update(b"T");
            write_bytes(hasher, text.as_bytes());
        }
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => {
            hasher.update(b"C");
            write_bytes(hasher, id.as_bytes());
            write_bytes(hasher, name.as_str().as_bytes());
            write_bytes(hasher, &canonical_map(arguments));
        }
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            hasher.update(b"R");
            write_bytes(hasher, call_id.as_bytes());
            write_bytes(hasher, content.as_bytes());
            hasher.update([u8::from(*is_error)]);
        }
        ContentBlock::Opaque { provider, data } => {
            hasher.update(b"O");
            write_bytes(hasher, provider.as_str().as_bytes());
            write_bytes(hasher, &canonical_map(data));
        }
    }
}

fn canonical_map(map: &serde_json::Map<String, Value>) -> Vec<u8> {
    let mut out = Vec::new();
    write_object(map.iter(), &mut out);
    out
}

/// Canonical JSON: object keys sorted by their UTF-8 bytes, no whitespace.
/// Strings and numbers are written by serde_json, which is deterministic for
/// both (`float_roundtrip` is on).
fn write_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => write_object(map.iter(), out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out);
            }
            out.push(b']');
        }
        // Scalars: serde_json's compact form.
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

fn write_object<'a>(entries: impl Iterator<Item = (&'a String, &'a Value)>, out: &mut Vec<u8>) {
    let mut entries: Vec<(&String, &Value)> = entries.collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    out.push(b'{');
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(Value::String(key.clone()).to_string().as_bytes());
        out.push(b':');
        write_value(value, out);
    }
    out.push(b'}');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn canonical(value: &Value) -> String {
        let mut out = Vec::new();
        write_value(value, &mut out);
        String::from_utf8(out).expect("canonical JSON is UTF-8")
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_depth() {
        let value = json!({ "b": [ { "z": 1, "a": 2 } ], "a": null, "c": "x\"y" });
        assert_eq!(
            canonical(&value),
            r#"{"a":null,"b":[{"a":2,"z":1}],"c":"x\"y"}"#
        );
        assert_eq!(canonical(&json!(1.5)), "1.5");
        assert_eq!(canonical(&json!([])), "[]");
        assert_eq!(canonical(&json!({})), "{}");
    }

    #[test]
    fn the_hash_is_a_sha256_string() {
        let hash = context_hash("system", &[]).expect("hash");
        assert!(hash.as_str().starts_with("sha256:"));
        assert_eq!(hash.as_str().len(), "sha256:".len() + 64);
    }

    #[test]
    fn boundaries_cannot_be_forged() {
        // Moving a byte from one text to the next must change the hash.
        let a = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "ab".into() },
                ContentBlock::Text { text: "c".into() },
            ],
        }];
        let b = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Text { text: "bc".into() },
            ],
        }];
        assert_ne!(
            context_hash("", &a).expect("hash"),
            context_hash("", &b).expect("hash")
        );
        // The system prompt and a text block are different things.
        assert_ne!(
            context_hash("x", &[]).expect("hash"),
            context_hash("", &[text(Role::User, "x")]).expect("hash")
        );
        // So are the roles.
        assert_ne!(
            context_hash("", &[text(Role::User, "x")]).expect("hash"),
            context_hash("", &[text(Role::Assistant, "x")]).expect("hash")
        );
    }

    #[test]
    fn map_key_order_does_not_matter_but_content_does() {
        let call = |arguments: Value| Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "measure".parse().expect("name"),
                arguments: arguments.as_object().cloned().expect("object"),
            }],
        };
        let one = call(json!({ "a": 1, "b": { "x": 1, "y": 2 } }));
        let same = call(json!({ "b": { "y": 2, "x": 1 }, "a": 1 }));
        let other = call(json!({ "a": 1, "b": { "x": 1, "y": 3 } }));
        let hash = |m: &Message| context_hash("", std::slice::from_ref(m)).expect("hash");
        assert_eq!(hash(&one), hash(&same));
        assert_ne!(hash(&one), hash(&other));
    }

    /// Pins the framing byte for byte: if this fails, the hash of every
    /// recorded context changed, and the `v1` prefix needs a bump.
    #[test]
    fn the_framing_is_exactly_the_documented_one() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "t".parse().expect("name"),
                        arguments: json!({ "b": 1, "a": 2 })
                            .as_object()
                            .cloned()
                            .expect("object"),
                    },
                    ContentBlock::Opaque {
                        provider: "p".parse().expect("id"),
                        data: json!({ "k": "v" }).as_object().cloned().expect("object"),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    content: "ok".into(),
                    is_error: true,
                }],
            },
        ];

        let mut preimage: Vec<u8> = Vec::new();
        let be = |n: u64| n.to_be_bytes();
        preimage.extend_from_slice(b"synth-context/v1\n");
        preimage.extend_from_slice(b"S");
        preimage.extend_from_slice(&be(3));
        preimage.extend_from_slice(b"sys");
        // user message, one block: Text "hi"
        preimage.extend_from_slice(b"MU");
        preimage.extend_from_slice(&be(1));
        preimage.extend_from_slice(b"T");
        preimage.extend_from_slice(&be(2));
        preimage.extend_from_slice(b"hi");
        // assistant message, two blocks
        preimage.extend_from_slice(b"MA");
        preimage.extend_from_slice(&be(2));
        preimage.extend_from_slice(b"C");
        preimage.extend_from_slice(&be(2));
        preimage.extend_from_slice(b"c1");
        preimage.extend_from_slice(&be(1));
        preimage.extend_from_slice(b"t");
        preimage.extend_from_slice(&be(13));
        preimage.extend_from_slice(br#"{"a":2,"b":1}"#);
        preimage.extend_from_slice(b"O");
        preimage.extend_from_slice(&be(1));
        preimage.extend_from_slice(b"p");
        preimage.extend_from_slice(&be(9));
        preimage.extend_from_slice(br#"{"k":"v"}"#);
        // user message: a failed tool result
        preimage.extend_from_slice(b"MU");
        preimage.extend_from_slice(&be(1));
        preimage.extend_from_slice(b"R");
        preimage.extend_from_slice(&be(2));
        preimage.extend_from_slice(b"c1");
        preimage.extend_from_slice(&be(2));
        preimage.extend_from_slice(b"ok");
        preimage.extend_from_slice(&[1]);

        let expected: String = Sha256::digest(&preimage)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let actual = context_hash("sys", &messages).expect("hash");
        assert_eq!(actual.as_str(), format!("sha256:{expected}"));
        // And the digest itself is frozen, so editing both sides of the
        // comparison above cannot hide a change. This value comes from an
        // independent implementation of the framing (Python's hashlib).
        assert_eq!(
            actual.as_str(),
            "sha256:e8b6c8a24c811ff84c02905da99f8d1af8aaf631df8ee1520ff3a43f5f89e592"
        );
    }
}
