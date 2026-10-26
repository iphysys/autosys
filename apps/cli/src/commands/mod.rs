//! The commands. Each sends its requests and prints the answer, as JSON or
//! as the human view; none decides anything a run does.

pub(crate) mod config;
pub(crate) mod doctor;
pub(crate) mod run;

use anyhow::Context as _;
use api_client::ClientError;
use clap::CommandFactory as _;

use crate::Failure;
use crate::args::{Cli, Command, ConfigCommand};
use crate::render::{human_stdout, write_plain};
use crate::settings::Settings;

pub(crate) async fn dispatch(cli: Cli) -> Result<(), Failure> {
    let settings = Settings::from_process(&cli.global);
    match cli.command {
        Command::Run(command) => run::run(command, &settings).await,
        Command::Config(ConfigCommand::Show) => config::show(&settings),
        Command::Doctor => doctor::doctor(&settings).await,
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "synth", &mut std::io::stdout());
            Ok(())
        }
    }
}

/// An API error, with the operation and the server it was sent to.
pub(crate) fn api_error<'a>(
    operation: &'a str,
    settings: &'a Settings,
) -> impl FnOnce(ClientError) -> Failure + 'a {
    move |error| {
        Failure::Other(
            anyhow::Error::from(error).context(format!("{operation} ({})", settings.api_url.value)),
        )
    }
}

/// The human output: plain lines, through the colour stream.
pub(crate) fn print_human(settings: &Settings, lines: &[String]) -> Result<(), Failure> {
    write_plain(&mut human_stdout(settings.color.value), lines)
        .context("cannot print the answer")?;
    Ok(())
}

/// The JSON output: the value as the API sent it, pretty-printed.
pub(crate) fn print_json(value: &impl serde::Serialize) -> Result<(), Failure> {
    let json = serde_json::to_string_pretty(value).context("cannot print the answer")?;
    println!("{json}");
    Ok(())
}
