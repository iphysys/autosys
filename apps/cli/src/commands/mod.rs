//! The commands. Each sends its requests and prints the answer, as JSON or
//! as the human view; none decides anything a run does.

pub(crate) mod artifact;
pub(crate) mod config;
pub(crate) mod doctor;
pub(crate) mod export;
pub(crate) mod run;
pub(crate) mod sdr;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use api_client::ClientError;
use api_client::ids::RunId;
use clap::CommandFactory as _;

use crate::Failure;
use crate::args::{Cli, Command, ConfigCommand};
use crate::render::{human_stdout, write_plain};
use crate::settings::Settings;

pub(crate) async fn dispatch(cli: Cli) -> Result<(), Failure> {
    let settings = Settings::from_process(&cli.global);
    match cli.command {
        Command::Run(command) => run::run(command, &settings).await,
        Command::Sdr(command) => sdr::sdr(command, &settings).await,
        Command::Artifact(command) => artifact::artifact(command, &settings).await,
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
    let json = json_text(value).context("cannot print the answer")?;
    print!("{json}");
    Ok(())
}

/// Writes `bytes` to `<path>.tmp` and renames it to `path`, so a reader
/// never sees half a file.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = Path::new(&temporary);
    let written = std::fs::File::create(temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    written
        .and_then(|()| std::fs::rename(temporary, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(temporary);
        })
        .with_context(|| format!("cannot write {}", path.display()))
}

/// The value as `print_json` prints it.
pub(crate) fn json_text(value: &impl serde::Serialize) -> anyhow::Result<String> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

/// The directory of a temporary journal (`watch --from-start`, `export`),
/// removed when dropped.
pub(crate) struct Replay(pub(crate) PathBuf);

impl Replay {
    fn new(purpose: &str, run: &RunId) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "synth-{purpose}-{}-{}",
            run.as_str(),
            std::process::id()
        ));
        // Left by an earlier process with the same id: start it afresh.
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
