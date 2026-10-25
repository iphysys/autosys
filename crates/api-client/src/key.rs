use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use contracts::ids::Identifier;
use sha2::{Digest, Sha256};

use crate::ClientError;

/// The `Idempotency-Key` of a run start (ADR-0018): the same key and
/// request return the same run, so a retry never starts a second one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// A key the caller chose, for example to retry an earlier start.
    pub fn new(key: &str) -> Result<Self, ClientError> {
        key.parse::<Identifier>()
            .map(|_| Self(key.to_owned()))
            .map_err(|e| {
                ClientError::InvalidInput(format!("`{key}` is not an idempotency key: {e}"))
            })
    }

    /// A key for a new run: a sha256 of the request, the time, the process
    /// and a counter, so two starts of the same request are two runs even
    /// within the clock's resolution.
    pub fn fresh(request: &[u8]) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut hash = Sha256::new();
        hash.update(request);
        hash.update(nanos.to_le_bytes());
        hash.update(std::process::id().to_le_bytes());
        hash.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        // A letter, then hex digits: always an identifier, which the tests
        // check.
        Self(format!("key-{hex}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_keys_are_valid_and_distinct() {
        let a = IdempotencyKey::fresh(b"{}");
        let b = IdempotencyKey::fresh(b"{}");
        assert_ne!(a, b);
        assert!(IdempotencyKey::new(a.as_str()).is_ok());
        assert!(a.as_str().starts_with("key-"));
    }

    #[test]
    fn a_chosen_key_must_be_an_identifier() {
        assert_eq!(
            IdempotencyKey::new("retry-1")
                .map(|k| k.as_str().to_owned())
                .ok(),
            Some("retry-1".to_owned())
        );
        for bad in ["", "-x", "a b", "a/b"] {
            assert!(
                matches!(IdempotencyKey::new(bad), Err(ClientError::InvalidInput(_))),
                "{bad:?}"
            );
        }
    }
}
