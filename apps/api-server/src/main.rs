//! The API server (ADR-0018): builds the API with bootstrap, then serves it
//! until a signal asks it to stop.
//!
//! ```text
//! api-server [--config <dir>] [--prompts <dir>]
//! ```
//!
//! Environment: `DATABASE_URL`, `SYNTH_API_TOKEN` (the development token,
//! ADR-0022), `SYNTH_ENVIRONMENT` and `RUST_LOG`. No value is ever logged.
//!
//! On Ctrl-C, SIGTERM, Ctrl-Break or a console close, the server stops
//! accepting connections, ends open event streams, lets the requests in
//! flight finish and exits. A second signal exits at once.
//!
//! Exit codes: 0 after a clean shutdown, 2 when the command line or a
//! startup check fails, 1 for anything unexpected.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use bootstrap::{ApiSetup, CancellationToken, ProcessEnv, Signals, build_api};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: api-server [--config <dir>] [--prompts <dir>]";

/// The exit code of a command line or startup failure.
const STARTUP_FAILED: u8 = 2;
/// The exit code of an unexpected failure, and of a second signal.
const UNEXPECTED: u8 = 1;

#[derive(Debug, PartialEq)]
struct Args {
    config: PathBuf,
    prompts: PathBuf,
}

impl Args {
    /// Reads the arguments after the program name.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self {
            config: PathBuf::from("config"),
            prompts: PathBuf::from("prompts"),
        };
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--config" => parsed.config = PathBuf::from(value()?),
                "--prompts" => parsed.prompts = PathBuf::from(value()?),
                other => return Err(format!("unknown argument `{other}`")),
            }
        }
        Ok(parsed)
    }
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("{problem}\n{USAGE}");
            return ExitCode::from(STARTUP_FAILED);
        }
    };
    match serve(args) {
        Ok(code) => code,
        Err(error) => {
            tracing::error!(error = format!("{error:#}"), "the api-server failed");
            ExitCode::from(UNEXPECTED)
        }
    }
}

fn serve(args: Args) -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    runtime.block_on(async {
        let shutdown = CancellationToken::new();
        let setup = ApiSetup {
            config_dir: args.config,
            prompts_dir: args.prompts,
        };
        let server = match build_api(setup, &ProcessEnv, shutdown.clone()).await {
            Ok(server) => server,
            Err(error) => {
                tracing::error!(%error, "the api-server cannot start");
                return Ok(ExitCode::from(STARTUP_FAILED));
            }
        };
        let listener = match tokio::net::TcpListener::bind(server.listen).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!(%error, listen = %server.listen, "cannot listen");
                return Ok(ExitCode::from(STARTUP_FAILED));
            }
        };
        let mut signals = Signals::listen().context("cannot listen for signals")?;
        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                signals.next().await;
                tracing::info!("shutting down: open streams end, requests in flight finish");
                shutdown.cancel();
                signals.next().await;
                tracing::warn!("a second signal: exiting at once");
                std::process::exit(i32::from(UNEXPECTED));
            }
        });
        tracing::info!(listen = %server.listen, environment = %server.environment, "api-server started");
        api::serve(listener, server.router, shutdown)
            .await
            .context("serving failed")?;
        tracing::info!("api-server exited cleanly");
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn defaults_are_the_repository_folders() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                config: PathBuf::from("config"),
                prompts: PathBuf::from("prompts"),
            })
        );
    }

    #[test]
    fn every_flag_takes_a_value() {
        assert_eq!(
            parse(&["--config", "c", "--prompts", "p"]),
            Ok(Args {
                config: PathBuf::from("c"),
                prompts: PathBuf::from("p"),
            })
        );
        assert_eq!(
            parse(&["--config"]),
            Err("--config needs a value".to_owned())
        );
        assert_eq!(
            parse(&["--id", "w"]),
            Err("unknown argument `--id`".to_owned())
        );
    }
}
