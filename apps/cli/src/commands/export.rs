//! `synth run export`: a run's report, every SDR version up to the final
//! one, its artifacts and its events, saved to a directory (ADR-0036). It
//! reads only; each file is written to a temporary name and renamed.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use api_client::ids::{RunId, SdrVersion};
use api_client::{Client, EventJournal};

use super::artifact::file_name;
use super::{Replay, api_error, json_text, print_human, write_atomically};
use crate::Failure;
use crate::render::design::export_summary;
use crate::settings::Settings;

/// Where `run export` writes unless `--out` says.
const DEFAULT_OUT: &str = "runtime-data/outputs";

pub(crate) async fn export(
    client: &Client,
    settings: &Settings,
    run: &RunId,
    out: Option<PathBuf>,
) -> Result<(), Failure> {
    let out = out.unwrap_or_else(|| Path::new(DEFAULT_OUT).join(run.as_str()));
    let report = client
        .report(run)
        .await
        .map_err(api_error("run export: the report", settings))?;
    std::fs::create_dir_all(&out).with_context(|| format!("cannot create {}", out.display()))?;
    let mut written = Vec::new();

    let path = out.join("report.json");
    write_atomically(&path, json_text(&report)?.as_bytes())?;
    written.push(path);

    let mut last = None;
    for version in 0..=report.final_sdr_version.0 {
        let view = client
            .sdr_version(&report.project_id, &SdrVersion(version))
            .await
            .map_err(api_error("run export: an SDR version", settings))?;
        let path = out.join(format!("sdr-v{version}.json"));
        write_atomically(&path, json_text(&view)?.as_bytes())?;
        written.push(path);
        last = Some(view);
    }

    if !report.artifacts.is_empty() {
        let dir = out.join("artifacts");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        for listed in &report.artifacts {
            let metadata = client
                .artifact(&listed.artifact_id)
                .await
                .map_err(api_error("run export: an artifact", settings))?;
            let name = file_name(listed.artifact_id.as_str(), metadata.format.as_str())?;
            let bytes = client
                .artifact_content(&listed.artifact_id)
                .await
                .map_err(api_error("run export: an artifact's content", settings))?;
            let path = dir.join(name);
            write_atomically(&path, &bytes)?;
            written.push(path);
        }
    }

    if report.final_ {
        let path = out.join("events.jsonl");
        write_atomically(&path, &events(client, settings, run).await?)?;
        written.push(path);
    } else {
        eprintln!(
            "events.jsonl skipped: the report is not final, so the run's events have not ended (it may be waiting for approval)"
        );
    }

    if settings.human() {
        let mut lines = vec![format!("Saved {} to {}", run.as_str(), out.display())];
        lines.extend(written.iter().map(|p| format!("  {}", p.display())));
        lines.push(String::new());
        lines.extend(export_summary(&report, last.as_ref()));
        print_human(settings, &lines)?;
    }
    Ok(())
}

/// Every event of a run whose stream ends, through a fresh temporary
/// journal: its contents once the stream has ended.
async fn events(client: &Client, settings: &Settings, run: &RunId) -> Result<Vec<u8>, Failure> {
    let replay = Replay::new("export", run);
    let path = replay.0.join("events.jsonl");
    {
        let mut journal = EventJournal::open(&path)?;
        let mut events = client.events(run, journal.last_seq());
        while let Some(event) = events
            .next()
            .await
            .map_err(api_error("run export: the events", settings))?
        {
            journal.append(&event)?;
        }
    }
    // A run with no events leaves no journal file.
    match std::fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("cannot read {}", path.display()))
            .into()),
    }
}
