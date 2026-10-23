//! Environment variables, read only at startup, and the secrets among them.

use std::collections::HashMap;
use std::fmt;

/// Where the gateway reads environment variables. Production passes
/// [`ProcessEnv`]; tests pass a map, so they never touch the process
/// environment.
pub trait Env {
    /// The variable's value. An empty or blank value counts as unset, so a
    /// line copied unchanged from `.env.example` does not pass as configured.
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        non_blank(std::env::var(name).ok())
    }
}

impl Env for HashMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        non_blank(self.get(name).cloned())
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// A credential. `Debug` never shows it, so it cannot reach a log or an error
/// message through formatting.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_values_count_as_unset() {
        let env: HashMap<String, String> = [
            ("SET".to_owned(), "value".to_owned()),
            ("EMPTY".to_owned(), String::new()),
            ("BLANK".to_owned(), "  ".to_owned()),
        ]
        .into();
        assert_eq!(env.var("SET").as_deref(), Some("value"));
        assert_eq!(env.var("EMPTY"), None);
        assert_eq!(env.var("BLANK"), None);
        assert_eq!(env.var("MISSING"), None);
    }

    #[test]
    fn debug_hides_the_secret() {
        let secret = Secret::new("sk-test-value");
        assert!(!format!("{secret:?}").contains("sk-test-value"));
        assert_eq!(secret.expose(), "sk-test-value");
    }
}
