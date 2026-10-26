//! `synth config show`: each setting and where it came from. The token is
//! reported as set or not, never printed.

use serde_json::json;

use super::{print_human, print_json};
use crate::Failure;
use crate::render::table;
use crate::settings::{STATE_DIR_ENV, Settings, Source, TOKEN_ENV, URL_ENV};

pub(crate) fn show(settings: &Settings) -> Result<(), Failure> {
    if !settings.human() {
        return print_json(&json!({
            "api_url": settings.api_url,
            "state_dir": settings.state_dir,
            "output": settings.output,
            "color": settings.color,
            "token_set": settings.token_set,
        }));
    }
    let source = |source: Source, env: &str| match source {
        Source::Environment => format!("environment ({env})"),
        Source::Default | Source::Flag => source.to_string(),
    };
    let token = if settings.token_set {
        ("set", format!("environment ({TOKEN_ENV})"))
    } else {
        ("not set", format!("needs {TOKEN_ENV}"))
    };
    let rows = vec![
        vec![
            "api url".to_owned(),
            settings.api_url.value.clone(),
            source(settings.api_url.source, URL_ENV),
        ],
        vec![
            "state dir".to_owned(),
            settings.state_dir.value.display().to_string(),
            source(settings.state_dir.source, STATE_DIR_ENV),
        ],
        vec![
            "output".to_owned(),
            settings.output.value.as_str().to_owned(),
            settings.output.source.to_string(),
        ],
        vec![
            "color".to_owned(),
            settings.color.value.as_str().to_owned(),
            settings.color.source.to_string(),
        ],
        vec!["token".to_owned(), token.0.to_owned(), token.1],
    ];
    print_human(settings, &table(&["setting", "value", "source"], &rows))
}
