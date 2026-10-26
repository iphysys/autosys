//! The synth CLI (ADR-0034, ADR-0036): a thin client of API v0. It sends
//! requests and prints what the server answers; the server decides
//! everything.
//!
//! ```text
//! synth [--api-url <url>] [--output human|json] [--color auto|always|never] [--state-dir <dir>] <command>
//!
//! run start   <request.json> [--idempotency-key <key>] [--watch]
//! run status  <run_id>
//! run watch   <run_id> [--from-start]
//! run approve <run_id> <approval_id> --answer <approve|reject> [--comment <text>]
//! run cancel  <run_id> [--reason <text>] [--yes]
//! run report  <run_id>
//! run export  <run_id> [--out <dir>]
//! sdr show    <project_id> <version>
//! artifact show <artifact_id>
//! artifact get  <artifact_id> [--out <file>]
//! config show
//! doctor
//! completions <shell>
//! ```
//!
//! Settings come from defaults < environment < flags: `SYNTH_API_URL`
//! (default `http://127.0.0.1:8080`) and `SYNTH_STATE_DIR` (default
//! `runtime-data/cli`). `SYNTH_API_TOKEN` holds the development token
//! (ADR-0022); it is never a flag and never printed. `RUST_LOG` filters the
//! logs on stderr.
//!
//! Output is `json` when stdout is not a terminal: the API's JSON body, and
//! one event per line for `watch`, as in ADR-0034. On a terminal it is
//! `human`, a short view of the same answers. `watch` journals each event
//! under `<state-dir>/runs/<run_id>/` before printing it, so run again it
//! resumes after the last event it received.
//!
//! Exit codes: 0 on success, 2 for a usage error, 1 for anything else,
//! including a refusal by the server, whose error goes to stderr.

mod args;
mod commands;
mod render;
mod settings;

use std::process::ExitCode;

use clap::Parser as _;
use tracing_subscriber::EnvFilter;

use crate::args::Cli;

/// The exit code of a usage error.
const USAGE_ERROR: u8 = 2;
/// The exit code of any other failure.
const FAILED: u8 = 1;

/// A failure, and whether it was the caller's usage.
pub(crate) enum Failure {
    Usage(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

impl From<api_client::ClientError> for Failure {
    fn from(error: api_client::ClientError) -> Self {
        Self::Other(error.into())
    }
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // Help and version go to stdout and exit 0; usage errors exit 2.
            let code = u8::try_from(error.exit_code()).unwrap_or(USAGE_ERROR);
            if error.print().is_err() {
                return ExitCode::from(FAILED);
            }
            return ExitCode::from(code);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::from(FAILED);
        }
    };
    match runtime.block_on(commands::dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(problem)) => {
            eprintln!("error: {problem}\n\nFor more information, try '--help'.");
            ExitCode::from(USAGE_ERROR)
        }
        Err(Failure::Other(error)) => {
            eprintln!("synth: {error:#}");
            ExitCode::from(FAILED)
        }
    }
}
