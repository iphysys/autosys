//! `synth run …`: start, status, watch, approve, cancel and report.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::Context as _;
use api_client::ids::{ApprovalId, RunId};
use api_client::types::{ApprovalAnswerRequest, CancelRequest, RunRequest};
use api_client::{Client, EventJournal, IdempotencyKey};

use super::{api_error, print_human, print_json};
use crate::Failure;
use crate::args::RunCommand;
use crate::render::event::Renderer;
use crate::render::report::{report_lines, status_lines};
use crate::render::{Tone, Zone, human_stdout};
use crate::settings::Settings;

pub(crate) async fn run(command: RunCommand, settings: &Settings) -> Result<(), Failure> {
    let client = settings.client()?;
    match command {
        RunCommand::Start {
            request,
            idempotency_key,
            watch: and_watch,
        } => {
            let bytes = std::fs::read(&request)
                .with_context(|| format!("cannot read {}", request.display()))?;
            let body: RunRequest = serde_json::from_slice(&bytes).map_err(|e| {
                Failure::Usage(format!("{} is not a run request: {e}", request.display()))
            })?;
            let key = match idempotency_key {
                Some(key) => {
                    IdempotencyKey::new(&key).map_err(|e| Failure::Usage(e.to_string()))?
                }
                None => {
                    let key = IdempotencyKey::fresh(&bytes);
                    eprintln!("idempotency key: {} (pass it again to retry)", key.as_str());
                    key
                }
            };
            let started = client
                .start_run(&body, &key)
                .await
                .map_err(api_error("run start", settings))?;
            if settings.human() {
                let what = if started.created {
                    "started"
                } else {
                    "was already started with this key"
                };
                print_human(
                    settings,
                    &[format!(
                        "{} {what}  (project {})",
                        started.run.run_id.as_str(),
                        started.run.project_id.as_str()
                    )],
                )?;
            } else {
                print_json(&started.run)?;
            }
            if and_watch {
                watch(&client, settings, &started.run.run_id, false).await?;
            }
            Ok(())
        }
        RunCommand::Status { run } => {
            let run = client
                .run(&parse_run(&run)?)
                .await
                .map_err(api_error("run status", settings))?;
            if settings.human() {
                print_human(settings, &status_lines(&run, Zone::Local))
            } else {
                print_json(&run)
            }
        }
        RunCommand::Watch { run, from_start } => {
            watch(&client, settings, &parse_run(&run)?, from_start).await
        }
        RunCommand::Approve {
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
            let request = ApprovalAnswerRequest {
                answer: answer.into(),
                comment,
            };
            let event = client
                .answer_approval(&parse_run(&run)?, &approval, &request)
                .await
                .map_err(api_error("run approve", settings))?;
            if settings.human() {
                print_human(
                    settings,
                    &[format!(
                        "answered {}: {}  (event {})",
                        approval.as_str(),
                        request.answer,
                        event.seq
                    )],
                )
            } else {
                print_json(&event)
            }
        }
        RunCommand::Cancel { run, reason, yes } => {
            let run = parse_run(&run)?;
            let reason = reason
                .map(|r| r.parse())
                .transpose()
                .map_err(|_| Failure::Usage("--reason is empty".to_owned()))?;
            let ask = should_confirm(
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                yes,
            );
            if ask && !confirm(&run)? {
                return print_human(settings, &["Not cancelled.".to_owned()]);
            }
            let event = client
                .cancel(&run, &CancelRequest { reason })
                .await
                .map_err(api_error("run cancel", settings))?;
            if settings.human() {
                print_human(
                    settings,
                    &[format!(
                        "cancel requested for {}  (event {})",
                        run.as_str(),
                        event.seq
                    )],
                )
            } else {
                print_json(&event)
            }
        }
        RunCommand::Report { run } => {
            let report = client
                .report(&parse_run(&run)?)
                .await
                .map_err(api_error("run report", settings))?;
            if settings.human() {
                print_human(settings, &report_lines(&report))
            } else {
                print_json(&report)
            }
        }
    }
}

/// Follows the run's events until the stream ends, journalling each before
/// printing it (ADR-0034). From the start, the events go to a fresh journal
/// that is deleted afterwards; the run's own journal is neither read nor
/// changed.
async fn watch(
    client: &Client,
    settings: &Settings,
    run: &RunId,
    from_start: bool,
) -> Result<(), Failure> {
    let replay = from_start.then(|| Replay::new(run));
    let path = match &replay {
        Some(replay) => replay.0.join("events.jsonl"),
        None => settings
            .state_dir
            .value
            .join("runs")
            .join(run.as_str())
            .join("events.jsonl"),
    };
    let mut journal = EventJournal::open(&path)?;
    let mut events = client.events(run, journal.last_seq());
    if !settings.human() {
        let mut stdout = std::io::stdout().lock();
        while let Some(event) = events
            .next()
            .await
            .map_err(api_error("run watch", settings))?
        {
            journal.append(&event)?;
            let line = serde_json::to_string(&event).context("cannot print an event")?;
            writeln!(stdout, "{line}")
                .and_then(|()| stdout.flush())
                .context("cannot print an event")?;
        }
        return Ok(());
    }
    let mut out = human_stdout(settings.color.value);
    let mut renderer = Renderer::new(Zone::Local);
    let info = Tone::Info.style();
    writeln!(
        out,
        "{info}Watching {} (Ctrl-C stops watching; the run carries on){info:#}",
        run.as_str()
    )
    .context("cannot print an event")?;
    while let Some(event) = events
        .next()
        .await
        .map_err(api_error("run watch", settings))?
    {
        journal.append(&event)?;
        for line in renderer.push(&event) {
            line.write(&mut out).context("cannot print an event")?;
        }
        out.flush().context("cannot print an event")?;
    }
    for line in renderer.finish() {
        line.write(&mut out).context("cannot print an event")?;
    }
    writeln!(
        out,
        "\n{info}Done. Full report: synth run report {}{info:#}",
        run.as_str()
    )
    .and_then(|()| out.flush())
    .context("cannot print an event")?;
    Ok(())
}

/// The directory of a `--from-start` journal, removed when dropped.
struct Replay(PathBuf);

impl Replay {
    fn new(run: &RunId) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "synth-watch-{}-{}",
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

/// Whether `run cancel` asks first: only when someone at a terminal can
/// answer, and `--yes` was not given (ADR-0036).
fn should_confirm(stdin_is_terminal: bool, stdout_is_terminal: bool, yes: bool) -> bool {
    stdin_is_terminal && stdout_is_terminal && !yes
}

/// Asks `Cancel <run_id>? [y/N]`; only `y` goes on.
fn confirm(run: &RunId) -> Result<bool, Failure> {
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "Cancel {}? [y/N] ", run.as_str())
        .and_then(|()| stdout.flush())
        .context("cannot ask for confirmation")?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("cannot read the confirmation")?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn parse_run(run: &str) -> Result<RunId, Failure> {
    run.parse()
        .map_err(|e| Failure::Usage(format!("`{run}` is not a run id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_asks_only_at_a_terminal_without_yes() {
        assert!(should_confirm(true, true, false));
        assert!(!should_confirm(true, true, true));
        assert!(!should_confirm(false, true, false));
        assert!(!should_confirm(true, false, false));
        assert!(!should_confirm(false, false, false));
        assert!(!should_confirm(false, false, true));
    }
}
