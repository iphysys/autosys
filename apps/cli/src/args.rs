//! The command line (ADR-0036): one clap definition gives the parser, the
//! help, the usage errors and the shell completions.

use std::path::PathBuf;

use api_client::types::ApprovalAnswer;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::render::{ColorMode, Output};

/// A thin client of the synth API: it sends requests and prints what the
/// server answers.
#[derive(Debug, PartialEq, Parser)]
#[command(name = "synth", bin_name = "synth", version)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: GlobalArgs,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Flags every command takes. Each overrides its environment variable,
/// which overrides the default (`settings`).
#[derive(Debug, Default, PartialEq, Args)]
pub(crate) struct GlobalArgs {
    /// The API server [env: SYNTH_API_URL] [default: http://127.0.0.1:8080]
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) api_url: Option<String>,
    /// human or json [default: human on a terminal, json otherwise]
    #[arg(long, global = true, value_enum)]
    pub(crate) output: Option<Output>,
    /// Colour of human output; NO_COLOR is honoured under auto [default: auto]
    #[arg(long, global = true, value_enum)]
    pub(crate) color: Option<ColorMode>,
    /// Where watch keeps its journals [env: SYNTH_STATE_DIR] [default: runtime-data/cli]
    #[arg(long, global = true, value_name = "DIR")]
    pub(crate) state_dir: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum Command {
    /// Start, follow, answer, cancel and report runs.
    #[command(subcommand)]
    Run(RunCommand),
    /// Show a committed version of a project's design.
    #[command(subcommand)]
    Sdr(SdrCommand),
    /// Show an artifact's metadata or fetch its bytes.
    #[command(subcommand)]
    Artifact(ArtifactCommand),
    /// The CLI's own settings.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Check that the server answers, the token is accepted and the state
    /// dir is writable. Reads only.
    Doctor,
    /// Print a completion script for a shell.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum RunCommand {
    /// Start a run from a run request file.
    Start {
        /// The run request (JSON).
        request: PathBuf,
        /// Pass the key printed by an earlier attempt to retry it.
        #[arg(long, value_name = "KEY")]
        idempotency_key: Option<String>,
        /// Watch the run once it is started.
        #[arg(long)]
        watch: bool,
    },
    /// Show a run's status.
    Status { run: String },
    /// Follow a run's events until its stream ends.
    Watch {
        run: String,
        /// Replay every event from the start, without touching the journal.
        #[arg(long)]
        from_start: bool,
    },
    /// Answer an approval the run waits for.
    Approve {
        run: String,
        approval: String,
        #[arg(long, value_enum)]
        answer: Answer,
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
    },
    /// Ask the server to cancel a run.
    Cancel {
        run: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show a run's report.
    Report { run: String },
    /// Save a run's report, design versions, artifacts and events to a
    /// directory.
    Export {
        run: String,
        /// Where to write [default: runtime-data/outputs/<run_id>]
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum SdrCommand {
    /// Show one committed version: its parameters and decisions.
    Show { project: String, version: u64 },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum ArtifactCommand {
    /// Show an artifact's metadata.
    Show { artifact: String },
    /// Fetch an artifact's bytes, to a file or to piped stdout.
    Get {
        artifact: String,
        /// The file to write; required when stdout is a terminal.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Print each setting and where it came from.
    Show,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Answer {
    Approve,
    Reject,
}

impl From<Answer> for ApprovalAnswer {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Approve => Self::Approve,
            Answer::Reject => Self::Reject,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("synth").chain(args.iter().copied()))
    }

    fn run(args: &[&str]) -> RunCommand {
        match parse(args).map(|cli| cli.command) {
            Ok(Command::Run(command)) => command,
            other => panic!("not a run command: {other:?}"),
        }
    }

    /// A usage error: its kind, and that it exits 2.
    fn usage_error(args: &[&str]) -> ErrorKind {
        match parse(args) {
            Ok(cli) => panic!("{args:?} parsed as {cli:?}"),
            Err(error) => {
                assert_eq!(error.exit_code(), 2, "{args:?}");
                error.kind()
            }
        }
    }

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_command_parses() {
        assert_eq!(
            run(&["run", "start", "req.json"]),
            RunCommand::Start {
                request: "req.json".into(),
                idempotency_key: None,
                watch: false
            }
        );
        assert_eq!(
            run(&[
                "run",
                "start",
                "req.json",
                "--idempotency-key",
                "k1",
                "--watch"
            ]),
            RunCommand::Start {
                request: "req.json".into(),
                idempotency_key: Some("k1".to_owned()),
                watch: true
            }
        );
        assert_eq!(
            run(&["run", "status", "run-1"]),
            RunCommand::Status {
                run: "run-1".to_owned()
            }
        );
        assert_eq!(
            run(&["run", "watch", "run-1"]),
            RunCommand::Watch {
                run: "run-1".to_owned(),
                from_start: false
            }
        );
        assert_eq!(
            run(&["run", "watch", "run-1", "--from-start"]),
            RunCommand::Watch {
                run: "run-1".to_owned(),
                from_start: true
            }
        );
        assert_eq!(
            run(&[
                "run",
                "approve",
                "run-1",
                "a-1",
                "--answer",
                "reject",
                "--comment",
                "no"
            ]),
            RunCommand::Approve {
                run: "run-1".to_owned(),
                approval: "a-1".to_owned(),
                answer: Answer::Reject,
                comment: Some("no".to_owned())
            }
        );
        assert_eq!(
            run(&["run", "approve", "run-1", "a-1", "--answer", "approve"]),
            RunCommand::Approve {
                run: "run-1".to_owned(),
                approval: "a-1".to_owned(),
                answer: Answer::Approve,
                comment: None
            }
        );
        assert_eq!(
            run(&["run", "cancel", "run-1", "--reason", "late", "--yes"]),
            RunCommand::Cancel {
                run: "run-1".to_owned(),
                reason: Some("late".to_owned()),
                yes: true
            }
        );
        assert_eq!(
            run(&["run", "report", "run-1"]),
            RunCommand::Report {
                run: "run-1".to_owned()
            }
        );
        assert_eq!(
            run(&["run", "export", "run-1"]),
            RunCommand::Export {
                run: "run-1".to_owned(),
                out: None
            }
        );
        assert_eq!(
            run(&["run", "export", "run-1", "--out", "o"]),
            RunCommand::Export {
                run: "run-1".to_owned(),
                out: Some("o".into())
            }
        );
        assert_eq!(
            parse(&["sdr", "show", "proj-1", "3"])
                .map(|c| c.command)
                .ok(),
            Some(Command::Sdr(SdrCommand::Show {
                project: "proj-1".to_owned(),
                version: 3
            }))
        );
        assert_eq!(
            parse(&["artifact", "show", "art-1"])
                .map(|c| c.command)
                .ok(),
            Some(Command::Artifact(ArtifactCommand::Show {
                artifact: "art-1".to_owned()
            }))
        );
        assert_eq!(
            parse(&["artifact", "get", "art-1", "--out", "f.step"])
                .map(|c| c.command)
                .ok(),
            Some(Command::Artifact(ArtifactCommand::Get {
                artifact: "art-1".to_owned(),
                out: Some("f.step".into())
            }))
        );
        assert_eq!(
            parse(&["config", "show"]).map(|c| c.command).ok(),
            Some(Command::Config(ConfigCommand::Show))
        );
        assert_eq!(
            parse(&["doctor"]).map(|c| c.command).ok(),
            Some(Command::Doctor)
        );
        assert_eq!(
            parse(&["completions", "powershell"])
                .map(|c| c.command)
                .ok(),
            Some(Command::Completions {
                shell: clap_complete::Shell::PowerShell
            })
        );
    }

    #[test]
    fn global_flags_parse_before_and_after_the_command() {
        let expected = GlobalArgs {
            api_url: Some("http://h:1".to_owned()),
            output: Some(Output::Json),
            color: Some(ColorMode::Never),
            state_dir: Some("s".into()),
        };
        let before = parse(&[
            "--api-url",
            "http://h:1",
            "--output",
            "json",
            "--color",
            "never",
            "--state-dir",
            "s",
            "run",
            "report",
            "run-1",
        ]);
        let after = parse(&[
            "run",
            "report",
            "run-1",
            "--api-url",
            "http://h:1",
            "--output",
            "json",
            "--color",
            "never",
            "--state-dir",
            "s",
        ]);
        assert_eq!(before.map(|c| c.global).ok(), Some(expected));
        assert_eq!(
            after.map(|c| c.global).ok(),
            parse(&[
                "--api-url",
                "http://h:1",
                "--output",
                "json",
                "--color",
                "never",
                "--state-dir",
                "s",
                "doctor"
            ])
            .map(|c| c.global)
            .ok()
        );
    }

    #[test]
    fn watch_still_takes_the_state_dir_after_its_run() {
        let cli = parse(&["run", "watch", "run-1", "--state-dir", "s"]).ok();
        assert_eq!(
            cli.as_ref().and_then(|c| c.global.state_dir.clone()),
            Some("s".into())
        );
        let cli = parse(&["run", "watch", "--state-dir", "s", "run-1"]).ok();
        assert_eq!(
            cli.map(|c| c.command),
            Some(Command::Run(RunCommand::Watch {
                run: "run-1".to_owned(),
                from_start: false
            }))
        );
    }

    #[test]
    fn mistakes_exit_2() {
        assert_eq!(
            usage_error(&[]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            usage_error(&["run", "start"]),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            usage_error(&["run", "approve", "run-1", "a-1"]),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            usage_error(&["run", "approve", "run-1", "a-1", "--answer", "maybe"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            usage_error(&["run", "report", "run-1", "--reason", "x"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            usage_error(&["run", "watch", "run-1", "--state-dir"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            usage_error(&["run", "cancel", "r", "--reason", "a", "--reason", "b"]),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            usage_error(&["run", "launch", "r"]),
            ErrorKind::InvalidSubcommand
        );
        assert_eq!(usage_error(&["deploy"]), ErrorKind::InvalidSubcommand);
        assert_eq!(
            usage_error(&["sdr", "show", "proj-1", "latest"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            usage_error(&["sdr", "show", "proj-1", "-1"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            usage_error(&["--output", "yaml", "doctor"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            usage_error(&["completions", "tcsh"]),
            ErrorKind::InvalidValue
        );
    }

    #[test]
    fn help_and_version_exit_0() {
        for args in [&["--help"][..], &["run", "--help"], &["--version"]] {
            let error = parse(args).err();
            assert_eq!(error.map(|e| e.exit_code()), Some(0), "{args:?}");
        }
    }

    #[test]
    fn there_is_no_token_flag() {
        assert_eq!(
            usage_error(&["run", "report", "run-1", "--token", "t"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            usage_error(&["--token", "t", "doctor"]),
            ErrorKind::UnknownArgument
        );
    }
}
