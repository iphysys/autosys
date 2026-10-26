//! `synth artifact show|get`: an artifact's metadata, or its bytes. Bytes
//! never go to a terminal.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::Context as _;
use api_client::ids::ArtifactId;

use super::{api_error, print_human, print_json, write_atomically};
use crate::Failure;
use crate::args::ArtifactCommand;
use crate::render::Zone;
use crate::render::design::artifact_lines;
use crate::settings::Settings;

/// The longest format used as a file extension.
const MAX_EXTENSION: usize = 16;

pub(crate) async fn artifact(command: ArtifactCommand, settings: &Settings) -> Result<(), Failure> {
    match command {
        ArtifactCommand::Show { artifact } => {
            let artifact = parse_artifact(&artifact)?;
            let metadata = settings
                .client()?
                .artifact(&artifact)
                .await
                .map_err(api_error("artifact show", settings))?;
            if settings.human() {
                print_human(settings, &artifact_lines(&metadata, Zone::Local))
            } else {
                print_json(&metadata)
            }
        }
        ArtifactCommand::Get { artifact, out } => {
            let artifact = parse_artifact(&artifact)?;
            let destination = destination(out, std::io::stdout().is_terminal())?;
            let bytes = settings
                .client()?
                .artifact_content(&artifact)
                .await
                .map_err(api_error("artifact get", settings))?;
            match destination {
                Destination::File(path) => {
                    write_atomically(&path, &bytes)?;
                    if settings.human() {
                        print_human(
                            settings,
                            &[format!("wrote {} bytes to {}", bytes.len(), path.display())],
                        )?;
                    }
                }
                Destination::Stdout => {
                    let mut stdout = std::io::stdout().lock();
                    stdout
                        .write_all(&bytes)
                        .and_then(|()| stdout.flush())
                        .context("cannot write the artifact to stdout")?;
                }
            }
            Ok(())
        }
    }
}

/// Where `artifact get` writes the bytes.
#[derive(Debug, PartialEq)]
enum Destination {
    File(PathBuf),
    Stdout,
}

/// `--out` if given; otherwise stdout, unless it is a terminal.
fn destination(out: Option<PathBuf>, stdout_is_terminal: bool) -> Result<Destination, Failure> {
    match out {
        Some(path) => Ok(Destination::File(path)),
        None if stdout_is_terminal => Err(Failure::Usage(
            "artifact get writes bytes: give --out <file>, or pipe stdout".to_owned(),
        )),
        None => Ok(Destination::Stdout),
    }
}

/// The file name of an artifact under `artifacts/` (ADR-0036): its id,
/// which must parse as an `ArtifactId`, then its format if that is 1–16
/// ASCII lowercase letters and digits, else `bin`.
pub(crate) fn file_name(artifact: &str, format: &str) -> anyhow::Result<String> {
    let id: ArtifactId = artifact
        .parse()
        .map_err(|e| anyhow::anyhow!("`{artifact}` is not an artifact id: {e}"))?;
    let usable = (1..=MAX_EXTENSION).contains(&format.len())
        && format
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let extension = if usable { format } else { "bin" };
    Ok(format!("{}.{extension}", id.as_str()))
}

fn parse_artifact(artifact: &str) -> Result<ArtifactId, Failure> {
    artifact
        .parse()
        .map_err(|e| Failure::Usage(format!("`{artifact}` is not an artifact id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_accepted_format_is_the_extension() {
        assert_eq!(file_name("art-1", "step").unwrap(), "art-1.step");
        assert_eq!(file_name("art-1", "stl").unwrap(), "art-1.stl");
        assert_eq!(file_name("art-1", "mp4").unwrap(), "art-1.mp4");
        assert_eq!(
            file_name("art-1", "abcdefghijklmnop").unwrap(),
            "art-1.abcdefghijklmnop"
        );
    }

    #[test]
    fn a_rejected_format_gives_bin() {
        for format in [
            "../x",
            "x/y",
            "x\\y",
            "STEP",
            "Step",
            "abcdefghijklmnopq",
            "",
            "st.ep",
            "stép",
        ] {
            assert_eq!(
                file_name("art-1", format).unwrap(),
                "art-1.bin",
                "{format:?}"
            );
        }
    }

    #[test]
    fn an_id_that_does_not_parse_is_refused() {
        for id in ["", "../art", "art/1", "a b", "-art"] {
            assert!(file_name(id, "step").is_err(), "{id:?}");
        }
    }

    #[test]
    fn get_without_out_is_a_usage_error_on_a_terminal() {
        assert!(matches!(destination(None, true), Err(Failure::Usage(_))));
        assert_eq!(destination(None, false).ok(), Some(Destination::Stdout));
        assert_eq!(
            destination(Some("f.step".into()), true).ok(),
            Some(Destination::File("f.step".into()))
        );
    }
}
