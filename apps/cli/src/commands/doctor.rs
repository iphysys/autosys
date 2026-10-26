//! `synth doctor` (ADR-0036): the server answers, the token is accepted and
//! the state dir is writable. It reads only and never creates a run.

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use api_client::ids::RunId;
use api_client::{ClientConfig, ClientError};

use super::print_json;
use crate::Failure;
use crate::render::{MAX_WIDTH, Tone, human_stdout, sanitize};
use crate::settings::{Settings, TOKEN_ENV};

/// Valid under the `RunId` pattern, but the server only issues `run-<hex>`
/// ids, so no run has it: a 404 means the token was accepted.
const PROBE_RUN: &str = "synth-doctor-probe";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, serde::Serialize)]
struct Check {
    check: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(check: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            check,
            status,
            detail: detail.into(),
        }
    }
}

pub(crate) async fn doctor(settings: &Settings) -> Result<(), Failure> {
    let (server, token) = probe(settings).await;
    let checks = [server, token, state_dir(&settings.state_dir.value)];
    if settings.human() {
        let mut out = human_stdout(settings.color.value);
        for check in &checks {
            let (word, tone) = match check.status {
                Status::Pass => ("ok", Tone::Good),
                Status::Fail => ("FAIL", Tone::Bad),
                Status::Skipped => ("skipped", Tone::Muted),
            };
            let style = tone.style();
            writeln!(
                out,
                "{style}{word:<8}{style:#}{:<10}{}",
                check.check,
                sanitize(&check.detail, MAX_WIDTH)
            )
            .context("cannot print the checks")?;
        }
        out.flush().context("cannot print the checks")?;
    } else {
        print_json(&checks)?;
    }
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    if failed > 0 {
        return Err(Failure::Other(anyhow::anyhow!(
            "{failed} of {} checks failed",
            checks.len()
        )));
    }
    Ok(())
}

/// The server and token checks, from one `GET /v0/runs/synth-doctor-probe`.
async fn probe(settings: &Settings) -> (Check, Check) {
    let url = &settings.api_url.value;
    let not_checked =
        |why: &str| Check::new("server", Status::Skipped, format!("not checked: {why}"));
    let Ok(token) = Settings::token() else {
        let why = if settings.token_set {
            format!("{TOKEN_ENV} does not hold a usable token")
        } else {
            format!("{TOKEN_ENV} is not set")
        };
        return (
            not_checked("no token to send"),
            Check::new("token", Status::Fail, why),
        );
    };
    // One attempt: a server that is down should be reported now.
    let config = ClientConfig {
        retries: 0,
        ..ClientConfig::default()
    };
    let client = match settings.client_with(token, config) {
        Ok(client) => client,
        Err(failure) => {
            let problem = match failure {
                Failure::Usage(problem) => problem,
                Failure::Other(error) => format!("{error:#}"),
            };
            return (
                Check::new("server", Status::Fail, problem),
                Check::new("token", Status::Skipped, "not checked: no server to ask"),
            );
        }
    };
    let probe: RunId = match PROBE_RUN.parse() {
        Ok(probe) => probe,
        Err(error) => {
            return (
                not_checked("no probe id"),
                Check::new("token", Status::Fail, format!("{PROBE_RUN}: {error}")),
            );
        }
    };
    let answers = || Check::new("server", Status::Pass, format!("answers at {url}"));
    match client.run(&probe).await {
        Err(ClientError::Api { status: 404, .. } | ClientError::Status { status: 404 }) => {
            (answers(), Check::new("token", Status::Pass, "accepted"))
        }
        Err(ClientError::Api { status: 401, .. } | ClientError::Status { status: 401 }) => (
            answers(),
            Check::new(
                "token",
                Status::Fail,
                "refused (401): synth and api-server hold different tokens",
            ),
        ),
        Err(error @ (ClientError::Transport(_) | ClientError::Timeout(_))) => (
            Check::new("server", Status::Fail, format!("{url}: {error}")),
            Check::new(
                "token",
                Status::Skipped,
                "not checked: the server did not answer",
            ),
        ),
        Err(error) => (
            answers(),
            Check::new("token", Status::Fail, format!("unexpected answer: {error}")),
        ),
        Ok(_) => (
            answers(),
            Check::new(
                "token",
                Status::Fail,
                format!("unexpected answer: a run named {PROBE_RUN} exists"),
            ),
        ),
    }
}

/// Whether `watch` can write its journals: a file is created in the state
/// dir, or in its nearest existing parent if it does not exist yet, and
/// removed again.
fn state_dir(dir: &Path) -> Check {
    let existing = dir
        .ancestors()
        .map(|a| {
            if a.as_os_str().is_empty() {
                Path::new(".")
            } else {
                a
            }
        })
        .find(|a| a.is_dir());
    let Some(existing) = existing else {
        return Check::new(
            "state dir",
            Status::Fail,
            format!("{}: no parent directory exists", dir.display()),
        );
    };
    let probe = existing.join(format!(".synth-doctor-{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|mut file| file.write_all(b"synth doctor\n"));
    let removed = std::fs::remove_file(&probe);
    match (written, removed) {
        (Ok(()), Ok(())) if existing == dir => Check::new(
            "state dir",
            Status::Pass,
            format!("{} is writable", dir.display()),
        ),
        (Ok(()), Ok(())) => Check::new(
            "state dir",
            Status::Pass,
            format!(
                "{} does not exist yet; {} is writable, so watch can create it",
                dir.display(),
                existing.display()
            ),
        ),
        (Err(error), _) | (Ok(()), Err(error)) => Check::new(
            "state dir",
            Status::Fail,
            format!("{}: {error}", existing.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_id_is_a_valid_run_id() {
        assert!(PROBE_RUN.parse::<RunId>().is_ok());
    }

    #[test]
    fn a_writable_state_dir_passes_and_is_left_unchanged() {
        let dir = std::env::temp_dir().join(format!("synth-doctor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let check = state_dir(&dir);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let missing = dir.join("not").join("yet");
        let check = state_dir(&missing);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(!missing.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
