//! The synth CLI (ADR-0034): a thin client of API v0. It sends requests and
//! prints what the server answers; the server decides everything.
//!
//! ```text
//! synth run start <request.json> [--idempotency-key <key>]
//! synth run watch <run_id> [--state-dir <dir>]
//! synth run approve <run_id> <approval_id> --answer <approve|reject> [--comment <text>]
//! synth run cancel <run_id> [--reason <text>]
//! synth run report <run_id>
//! ```
//!
//! Environment: `SYNTH_API_URL` (default `http://127.0.0.1:8080`),
//! `SYNTH_API_TOKEN` (the development token, ADR-0022; never a flag, never
//! printed) and `RUST_LOG`.
//!
//! Output is the API's JSON body on stdout; `watch` prints one event per
//! line and journals each first under `<state-dir>/runs/<run_id>/`, so run
//! again it resumes after the last event it received.
//!
//! Exit codes: 0 on success, 2 for a usage error, 1 for anything else,
//! including a refusal by the server, whose error goes to stderr.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use api_client::ids::{ApprovalId, RunId};
use api_client::types::{ApprovalAnswer, ApprovalAnswerRequest, CancelRequest, RunRequest};
use api_client::{Client, ClientConfig, EventJournal, IdempotencyKey, Token};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
usage: synth run start <request.json> [--idempotency-key <key>]
       synth run watch <run_id> [--state-dir <dir>]
       synth run approve <run_id> <approval_id> --answer <approve|reject> [--comment <text>]
       synth run cancel <run_id> [--reason <text>]
       synth run report <run_id>";

const URL_ENV: &str = "SYNTH_API_URL";
const TOKEN_ENV: &str = "SYNTH_API_TOKEN";
/// api-server's listen address in the committed `config/api.yaml`.
const DEFAULT_URL: &str = "http://127.0.0.1:8080";
/// Where `watch` keeps its journals unless `--state-dir` says otherwise.
const DEFAULT_STATE_DIR: &str = "runtime-data/cli";

/// The exit code of a usage error.
const USAGE_ERROR: u8 = 2;
/// The exit code of any other failure.
const FAILED: u8 = 1;

#[derive(Debug, PartialEq)]
enum Command {
    Start {
        request: PathBuf,
        key: Option<String>,
    },
    Watch {
        run: String,
        state_dir: PathBuf,
    },
    Approve {
        run: String,
        approval: String,
        answer: ApprovalAnswer,
        comment: Option<String>,
    },
    Cancel {
        run: String,
        reason: Option<String>,
    },
    Report {
        run: String,
    },
}

impl Command {
    /// Reads the arguments after the program name.
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut options: Vec<(String, String)> = Vec::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if let Some(name) = arg.strip_prefix("--") {
                let value = args
                    .next_if(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                if options.iter().any(|(n, _)| n == name) {
                    return Err(format!("--{name} is given twice"));
                }
                options.push((name.to_owned(), value));
            } else {
                positional.push(arg);
            }
        }
        let mut take = |name: &str| {
            options
                .iter()
                .position(|(n, _)| n == name)
                .map(|at| options.remove(at).1)
        };
        let positional: Vec<&str> = positional.iter().map(String::as_str).collect();
        let command = match positional.as_slice() {
            ["run", "start", request] => Self::Start {
                request: PathBuf::from(request),
                key: take("idempotency-key"),
            },
            ["run", "watch", run] => Self::Watch {
                run: (*run).to_owned(),
                state_dir: take("state-dir")
                    .map_or_else(|| DEFAULT_STATE_DIR.into(), PathBuf::from),
            },
            ["run", "approve", run, approval] => Self::Approve {
                run: (*run).to_owned(),
                approval: (*approval).to_owned(),
                answer: match take("answer").as_deref() {
                    Some("approve") => ApprovalAnswer::Approve,
                    Some("reject") => ApprovalAnswer::Reject,
                    Some(other) => {
                        return Err(format!("--answer is approve or reject, not `{other}`"));
                    }
                    None => return Err("--answer is required".to_owned()),
                },
                comment: take("comment"),
            },
            ["run", "cancel", run] => Self::Cancel {
                run: (*run).to_owned(),
                reason: take("reason"),
            },
            ["run", "report", run] => Self::Report {
                run: (*run).to_owned(),
            },
            [] => return Err("no command given".to_owned()),
            other => return Err(format!("unknown command `{}`", other.join(" "))),
        };
        if let Some((name, _)) = options.first() {
            return Err(format!("unknown option --{name} for this command"));
        }
        Ok(command)
    }
}

/// A failure, and whether it was the caller's usage.
enum Failure {
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
    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(problem) => {
            eprintln!("{problem}\n{USAGE}");
            return ExitCode::from(USAGE_ERROR);
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
    match runtime.block_on(run(command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(problem)) => {
            eprintln!("{problem}\n{USAGE}");
            ExitCode::from(USAGE_ERROR)
        }
        Err(Failure::Other(error)) => {
            eprintln!("synth: {error:#}");
            ExitCode::from(FAILED)
        }
    }
}

async fn run(command: Command) -> Result<(), Failure> {
    let client = client()?;
    match command {
        Command::Start { request, key } => {
            let bytes = std::fs::read(&request)
                .with_context(|| format!("cannot read {}", request.display()))?;
            let body: RunRequest = serde_json::from_slice(&bytes).map_err(|e| {
                Failure::Usage(format!("{} is not a run request: {e}", request.display()))
            })?;
            let key = match key {
                Some(key) => {
                    IdempotencyKey::new(&key).map_err(|e| Failure::Usage(e.to_string()))?
                }
                None => {
                    let key = IdempotencyKey::fresh(&bytes);
                    eprintln!("idempotency key: {} (pass it again to retry)", key.as_str());
                    key
                }
            };
            print_json(&client.start_run(&body, &key).await?.run)
        }
        Command::Watch { run, state_dir } => {
            let run = parse_run(&run)?;
            let path = state_dir
                .join("runs")
                .join(run.as_str())
                .join("events.jsonl");
            let mut journal = EventJournal::open(&path)?;
            let mut events = client.events(&run, journal.last_seq());
            let mut stdout = std::io::stdout().lock();
            while let Some(event) = events.next().await? {
                journal.append(&event)?;
                let line = serde_json::to_string(&event).context("cannot print an event")?;
                writeln!(stdout, "{line}")
                    .and_then(|()| stdout.flush())
                    .context("cannot print an event")?;
            }
            Ok(())
        }
        Command::Approve {
            run,
            approval,
            answer,
            comment,
        } => {
            let approval: ApprovalId = approval
                .parse()
                .map_err(|e| Failure::Usage(format!("`{approval}` is not an approval id: {e}")))?;
            let comment = comment
                .map(|c| c.parse())
                .transpose()
                .map_err(|_| Failure::Usage("--comment is empty".to_owned()))?;
            let request = ApprovalAnswerRequest { answer, comment };
            print_json(
                &client
                    .answer_approval(&parse_run(&run)?, &approval, &request)
                    .await?,
            )
        }
        Command::Cancel { run, reason } => {
            let reason = reason
                .map(|r| r.parse())
                .transpose()
                .map_err(|_| Failure::Usage("--reason is empty".to_owned()))?;
            print_json(
                &client
                    .cancel(&parse_run(&run)?, &CancelRequest { reason })
                    .await?,
            )
        }
        Command::Report { run } => print_json(&client.report(&parse_run(&run)?).await?),
    }
}

/// The client of the server `SYNTH_API_URL` names, with the token of
/// `SYNTH_API_TOKEN`.
fn client() -> Result<Client, Failure> {
    let url = std::env::var(URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .and_then(|t| Token::new(t).ok())
        .ok_or_else(|| Failure::Usage(format!("{TOKEN_ENV} must hold the API token")))?;
    Client::new(&url, token, ClientConfig::default())
        .map_err(|e| Failure::Usage(format!("{URL_ENV}: {e}")))
}

fn parse_run(run: &str) -> Result<RunId, Failure> {
    run.parse()
        .map_err(|e| Failure::Usage(format!("`{run}` is not a run id: {e}")))
}

fn print_json(value: &impl serde::Serialize) -> Result<(), Failure> {
    let json = serde_json::to_string_pretty(value).context("cannot print the answer")?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        Command::parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn every_command_parses() {
        assert_eq!(
            parse(&["run", "start", "req.json"]),
            Ok(Command::Start {
                request: "req.json".into(),
                key: None
            })
        );
        assert_eq!(
            parse(&["run", "start", "req.json", "--idempotency-key", "k1"]),
            Ok(Command::Start {
                request: "req.json".into(),
                key: Some("k1".to_owned())
            })
        );
        assert_eq!(
            parse(&["run", "watch", "run-1"]),
            Ok(Command::Watch {
                run: "run-1".to_owned(),
                state_dir: DEFAULT_STATE_DIR.into()
            })
        );
        assert_eq!(
            parse(&["run", "watch", "--state-dir", "s", "run-1"]),
            Ok(Command::Watch {
                run: "run-1".to_owned(),
                state_dir: "s".into()
            })
        );
        assert_eq!(
            parse(&[
                "run",
                "approve",
                "run-1",
                "a-1",
                "--answer",
                "reject",
                "--comment",
                "no"
            ]),
            Ok(Command::Approve {
                run: "run-1".to_owned(),
                approval: "a-1".to_owned(),
                answer: ApprovalAnswer::Reject,
                comment: Some("no".to_owned())
            })
        );
        assert_eq!(
            parse(&["run", "cancel", "run-1", "--reason", "late"]),
            Ok(Command::Cancel {
                run: "run-1".to_owned(),
                reason: Some("late".to_owned())
            })
        );
        assert_eq!(
            parse(&["run", "report", "run-1"]),
            Ok(Command::Report {
                run: "run-1".to_owned()
            })
        );
    }

    #[test]
    fn mistakes_are_named() {
        assert_eq!(parse(&[]), Err("no command given".to_owned()));
        assert_eq!(
            parse(&["run", "start"]),
            Err("unknown command `run start`".to_owned())
        );
        assert_eq!(
            parse(&["run", "approve", "run-1", "a-1"]),
            Err("--answer is required".to_owned())
        );
        assert_eq!(
            parse(&["run", "approve", "run-1", "a-1", "--answer", "maybe"]),
            Err("--answer is approve or reject, not `maybe`".to_owned())
        );
        assert_eq!(
            parse(&["run", "report", "run-1", "--reason", "x"]),
            Err("unknown option --reason for this command".to_owned())
        );
        assert_eq!(
            parse(&["run", "watch", "run-1", "--state-dir"]),
            Err("--state-dir needs a value".to_owned())
        );
        assert_eq!(
            parse(&["run", "cancel", "r", "--reason", "a", "--reason", "b"]),
            Err("--reason is given twice".to_owned())
        );
    }

    #[test]
    fn there_is_no_token_flag() {
        assert_eq!(
            parse(&["run", "report", "run-1", "--token", "t"]),
            Err("unknown option --token for this command".to_owned())
        );
    }
}
