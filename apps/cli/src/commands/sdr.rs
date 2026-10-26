//! `synth sdr show`: one committed version of a project's design.

use api_client::ids::{ProjectId, SdrVersion};

use super::{api_error, print_human, print_json};
use crate::Failure;
use crate::args::SdrCommand;
use crate::render::Zone;
use crate::render::design::sdr_lines;
use crate::settings::Settings;

pub(crate) async fn sdr(command: SdrCommand, settings: &Settings) -> Result<(), Failure> {
    let SdrCommand::Show { project, version } = command;
    let project: ProjectId = project
        .parse()
        .map_err(|e| Failure::Usage(format!("`{project}` is not a project id: {e}")))?;
    let view = settings
        .client()?
        .sdr_version(&project, &SdrVersion(version))
        .await
        .map_err(api_error("sdr show", settings))?;
    if settings.human() {
        print_human(settings, &sdr_lines(&view, Zone::Local))
    } else {
        print_json(&view)
    }
}
