//! The CLI's settings (blueprint §11.1, ADR-0036): defaults < environment <
//! flags, each with where it came from.

use std::fmt;
use std::io::IsTerminal as _;
use std::path::PathBuf;

use api_client::{Client, ClientConfig, Token};

use crate::Failure;
use crate::args::GlobalArgs;
use crate::render::{self, ColorMode, Output};

pub(crate) const URL_ENV: &str = "SYNTH_API_URL";
pub(crate) const TOKEN_ENV: &str = "SYNTH_API_TOKEN";
pub(crate) const STATE_DIR_ENV: &str = "SYNTH_STATE_DIR";
/// api-server's listen address in the committed `config/api.yaml`.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";
/// Where `watch` keeps its journals unless told otherwise.
const DEFAULT_STATE_DIR: &str = "runtime-data/cli";

/// Where a setting's value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Default,
    Environment,
    Flag,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Environment => "environment",
            Self::Flag => "flag",
        })
    }
}

/// A setting's value and where it came from.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct Sourced<T> {
    pub(crate) value: T,
    pub(crate) source: Source,
}

impl<T> Sourced<T> {
    /// The flag if given, else the environment's value, else the default.
    fn pick(flag: Option<T>, environment: Option<T>, default: T) -> Self {
        match (flag, environment) {
            (Some(value), _) => Self {
                value,
                source: Source::Flag,
            },
            (None, Some(value)) => Self {
                value,
                source: Source::Environment,
            },
            (None, None) => Self {
                value: default,
                source: Source::Default,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub(crate) api_url: Sourced<String>,
    pub(crate) state_dir: Sourced<PathBuf>,
    pub(crate) output: Sourced<Output>,
    pub(crate) color: Sourced<ColorMode>,
    /// Whether `SYNTH_API_TOKEN` holds something; the token itself is read
    /// only when a client is built.
    pub(crate) token_set: bool,
}

impl Settings {
    /// The settings for these flags, given the environment and whether
    /// stdout is a terminal. An empty variable counts as unset.
    pub(crate) fn resolve(
        flags: &GlobalArgs,
        env: impl Fn(&str) -> Option<String>,
        stdout_is_terminal: bool,
    ) -> Self {
        let env = |name: &str| env(name).filter(|v| !v.is_empty());
        Self {
            api_url: Sourced::pick(flags.api_url.clone(), env(URL_ENV), DEFAULT_URL.to_owned()),
            state_dir: Sourced::pick(
                flags.state_dir.clone(),
                env(STATE_DIR_ENV).map(PathBuf::from),
                DEFAULT_STATE_DIR.into(),
            ),
            output: Sourced::pick(
                flags.output,
                None,
                render::resolve_output(None, stdout_is_terminal),
            ),
            color: Sourced::pick(flags.color, None, ColorMode::Auto),
            token_set: env(TOKEN_ENV).is_some(),
        }
    }

    /// The settings of this process.
    pub(crate) fn from_process(flags: &GlobalArgs) -> Self {
        Self::resolve(
            flags,
            |name| std::env::var(name).ok(),
            std::io::stdout().is_terminal(),
        )
    }

    pub(crate) fn human(&self) -> bool {
        self.output.value == Output::Human
    }

    /// The client of the server `api_url` names, with the token of
    /// `SYNTH_API_TOKEN`.
    pub(crate) fn client(&self) -> Result<Client, Failure> {
        self.client_with(Self::token()?, ClientConfig::default())
    }

    /// The token `SYNTH_API_TOKEN` holds.
    pub(crate) fn token() -> Result<Token, Failure> {
        std::env::var(TOKEN_ENV)
            .ok()
            .and_then(|t| Token::new(t).ok())
            .ok_or_else(|| Failure::Usage(format!("{TOKEN_ENV} must hold the API token")))
    }

    pub(crate) fn client_with(
        &self,
        token: Token,
        config: ClientConfig,
    ) -> Result<Client, Failure> {
        Client::new(&self.api_url.value, token, config)
            .map_err(|e| Failure::Usage(format!("the API URL ({}): {e}", self.api_url.source)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    fn flags(api_url: Option<&str>, state_dir: Option<&str>) -> GlobalArgs {
        GlobalArgs {
            api_url: api_url.map(str::to_owned),
            state_dir: state_dir.map(PathBuf::from),
            ..GlobalArgs::default()
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_given() {
        let settings = Settings::resolve(&flags(None, None), env(&[]), true);
        assert_eq!(
            settings.api_url,
            Sourced {
                value: DEFAULT_URL.to_owned(),
                source: Source::Default
            }
        );
        assert_eq!(
            settings.state_dir,
            Sourced {
                value: DEFAULT_STATE_DIR.into(),
                source: Source::Default
            }
        );
        assert_eq!(settings.color.value, ColorMode::Auto);
        assert!(!settings.token_set);
    }

    #[test]
    fn the_environment_overrides_the_default() {
        let vars = [(URL_ENV, "http://env:1"), (STATE_DIR_ENV, "env-dir")];
        let settings = Settings::resolve(&flags(None, None), env(&vars), true);
        assert_eq!(
            settings.api_url,
            Sourced {
                value: "http://env:1".to_owned(),
                source: Source::Environment
            }
        );
        assert_eq!(
            settings.state_dir,
            Sourced {
                value: "env-dir".into(),
                source: Source::Environment
            }
        );
    }

    #[test]
    fn a_flag_overrides_the_environment() {
        let vars = [(URL_ENV, "http://env:1"), (STATE_DIR_ENV, "env-dir")];
        let settings = Settings::resolve(
            &flags(Some("http://flag:2"), Some("flag-dir")),
            env(&vars),
            true,
        );
        assert_eq!(
            settings.api_url,
            Sourced {
                value: "http://flag:2".to_owned(),
                source: Source::Flag
            }
        );
        assert_eq!(
            settings.state_dir,
            Sourced {
                value: "flag-dir".into(),
                source: Source::Flag
            }
        );
    }

    #[test]
    fn an_empty_variable_is_unset() {
        let vars = [(URL_ENV, ""), (TOKEN_ENV, "")];
        let settings = Settings::resolve(&flags(None, None), env(&vars), true);
        assert_eq!(settings.api_url.source, Source::Default);
        assert!(!settings.token_set);
        let settings = Settings::resolve(&flags(None, None), env(&[(TOKEN_ENV, "t")]), true);
        assert!(settings.token_set);
    }

    #[test]
    fn output_follows_the_terminal_unless_a_flag_says() {
        let on_terminal = Settings::resolve(&flags(None, None), env(&[]), true);
        assert_eq!(
            on_terminal.output,
            Sourced {
                value: Output::Human,
                source: Source::Default
            }
        );
        let piped = Settings::resolve(&flags(None, None), env(&[]), false);
        assert_eq!(piped.output.value, Output::Json);
        let forced = GlobalArgs {
            output: Some(Output::Human),
            ..GlobalArgs::default()
        };
        assert_eq!(
            Settings::resolve(&forced, env(&[]), false).output,
            Sourced {
                value: Output::Human,
                source: Source::Flag
            }
        );
    }
}
