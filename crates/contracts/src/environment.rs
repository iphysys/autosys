//! Where the harness runs. Configuration rules that differ by place, such as
//! the floors on worker intervals or plain-http test endpoints, take it as
//! an argument; nothing reads it from a global (ADR-0031).

use std::fmt;

use crate::error::{Error, ErrorCode};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Environment {
    /// A developer's machine.
    Local,
    /// Automated tests. Only here may intervals be short and model
    /// endpoints plain http on a loopback host.
    Test,
    /// Everything else, and the default: the committed `config/` is
    /// production configuration.
    Production,
}

impl Environment {
    /// The environment variable that names the environment.
    pub const VAR: &'static str = "SYNTH_ENVIRONMENT";

    /// The environment `value` names, as `SYNTH_ENVIRONMENT` holds it: unset
    /// is Production; an unknown name is InvalidInput.
    pub fn from_var(value: Option<&str>) -> Result<Self, Error> {
        match value {
            None => Ok(Environment::Production),
            Some("local") => Ok(Environment::Local),
            Some("test") => Ok(Environment::Test),
            Some("production") => Ok(Environment::Production),
            Some(other) => Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "{} is `{other}`; it must be local, test or production",
                    Self::VAR
                ),
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Environment::Local => "local",
            Environment::Test => "test",
            Environment::Production => "production",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_production_and_names_round_trip() {
        assert_eq!(
            Environment::from_var(None).ok(),
            Some(Environment::Production)
        );
        for env in [
            Environment::Local,
            Environment::Test,
            Environment::Production,
        ] {
            assert_eq!(Environment::from_var(Some(env.as_str())).ok(), Some(env));
        }
    }

    #[test]
    fn an_unknown_name_is_refused() {
        let error = Environment::from_var(Some("staging")).expect_err("unknown");
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        assert!(error.message().contains("SYNTH_ENVIRONMENT"), "{error}");
        assert!(
            Environment::from_var(Some("Test")).is_err(),
            "names are lower case"
        );
    }
}
