//! The worker process (ADR-0031): builds a worker with bootstrap, then
//! claims and runs jobs until a signal asks it to stop.
//!
//! ```text
//! worker [--config <dir>] [--prompts <dir>] [--id <id>]
//! ```
//!
//! Environment: `DATABASE_URL`, the model keys `config/models/routing.yaml`
//! names, `SYNTH_MODEL_PROFILE`, `SYNTH_ENVIRONMENT`, `SYNTH_WORKER_ID` and
//! `RUST_LOG`. No value is ever logged.
//!
//! On Ctrl-C, SIGTERM, Ctrl-Break or a console close, the worker stops
//! claiming, gives the job it runs back to the queue (not a failed
//! attempt), waits for its tasks and exits. A second signal exits at once.
//!
//! Exit codes: 0 after a clean shutdown, 2 when the command line or a
//! startup check fails, 1 for anything unexpected.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use bootstrap::{CancellationToken, Overrides, ProcessEnv, WorkerSetup, build_worker};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: worker [--config <dir>] [--prompts <dir>] [--id <id>]";

/// The exit code of a command line or startup failure.
const STARTUP_FAILED: u8 = 2;
/// The exit code of an unexpected failure, and of a second signal.
const UNEXPECTED: u8 = 1;

#[derive(Debug, PartialEq)]
struct Args {
    config: PathBuf,
    prompts: PathBuf,
    id: Option<String>,
}

impl Args {
    /// Reads the arguments after the program name.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self {
            config: PathBuf::from("config"),
            prompts: PathBuf::from("prompts"),
            id: None,
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
                "--id" => parsed.id = Some(value()?),
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
            tracing::error!(error = format!("{error:#}"), "the worker failed");
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
        let setup = WorkerSetup {
            config_dir: args.config,
            prompts_dir: args.prompts,
            worker_id: args.id,
            overrides: Overrides::default(),
        };
        let worker = match build_worker(setup, &ProcessEnv).await {
            Ok(worker) => worker,
            Err(error) => {
                tracing::error!(%error, "the worker cannot start");
                return Ok(ExitCode::from(STARTUP_FAILED));
            }
        };
        let mut signals = Signals::listen().context("cannot listen for signals")?;
        let shutdown = CancellationToken::new();
        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                signals.next().await;
                tracing::info!("shutting down: the running job goes back to the queue");
                shutdown.cancel();
                signals.next().await;
                tracing::warn!("a second signal: exiting at once");
                std::process::exit(i32::from(UNEXPECTED));
            }
        });
        worker.run(shutdown).await;
        worker.drain().await;
        tracing::info!(worker = worker.id.as_str(), "worker exited cleanly");
        Ok(ExitCode::SUCCESS)
    })
}

/// The signals that ask the worker to stop.
struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
}

impl Signals {
    fn listen() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(windows)]
            ctrl_break: tokio::signal::windows::ctrl_break()?,
            #[cfg(windows)]
            close: tokio::signal::windows::ctrl_close()?,
        })
    }

    /// Waits for the next signal. If listening fails, that counts as one,
    /// so the worker still stops.
    async fn next(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(windows)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = self.ctrl_break.recv() => {}
            _ = self.close.recv() => {}
        }
    }
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
                id: None
            })
        );
    }

    #[test]
    fn every_flag_takes_a_value() {
        assert_eq!(
            parse(&["--config", "c", "--prompts", "p", "--id", "w-1"]),
            Ok(Args {
                config: PathBuf::from("c"),
                prompts: PathBuf::from("p"),
                id: Some("w-1".to_owned())
            })
        );
        assert_eq!(parse(&["--id"]), Err("--id needs a value".to_owned()));
        assert_eq!(
            parse(&["--config", "--id", "w"]),
            Err("--config needs a value".to_owned())
        );
        assert_eq!(
            parse(&["--verbose"]),
            Err("unknown argument `--verbose`".to_owned())
        );
    }
}
