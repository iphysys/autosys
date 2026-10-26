//! Output (ADR-0036): `json` is the API's body, unchanged; `human` is a
//! short view of it. Text from the model, a tool or the server is untrusted:
//! it goes through `sanitize` before it is printed.

pub(crate) mod design;
pub(crate) mod event;
pub(crate) mod report;

use std::io::Write;

use api_client::types::{JobStatus, Timestamp};
use clap::ValueEnum;
use clap::builder::styling::{AnsiColor, Style};

/// The longest text one field of the human view prints.
pub(crate) const MAX_WIDTH: usize = 200;

/// The time zone times are shown in: local time, or a fixed offset for
/// tests.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Zone {
    Local,
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(chrono::FixedOffset),
}

impl Zone {
    /// `at` in this zone, formatted as `chrono` formats.
    pub(crate) fn format(self, at: &Timestamp, format: &str) -> String {
        match self {
            Self::Local => {
                at.0.with_timezone(&chrono::Local)
                    .format(format)
                    .to_string()
            }
            Self::Fixed(offset) => at.0.with_timezone(&offset).format(format).to_string(),
        }
    }
}

/// A job status's state, as the schema names it.
pub(crate) fn state_name(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Queued { .. } => "queued",
        JobStatus::Running => "running",
        JobStatus::Waiting { .. } => "waiting",
        JobStatus::Suspended { .. } => "suspended",
        JobStatus::Completed { .. } => "completed",
        JobStatus::Cancelled { .. } => "cancelled",
        JobStatus::Failed { .. } => "failed",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Human,
    Json,
}

impl Output {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
        }
    }
}

/// The output for `--output`, given whether stdout is a terminal: `human`
/// on a terminal and `json` otherwise, unless the flag says.
pub(crate) fn resolve_output(flag: Option<Output>, stdout_is_terminal: bool) -> Output {
    match flag {
        Some(output) => output,
        None if stdout_is_terminal => Output::Human,
        None => Output::Json,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

impl From<ColorMode> for anstream::ColorChoice {
    fn from(mode: ColorMode) -> Self {
        match mode {
            ColorMode::Auto => Self::Auto,
            ColorMode::Always => Self::Always,
            ColorMode::Never => Self::Never,
        }
    }
}

/// Stdout for human output: styles are written as `--color` says, and
/// stripped under `auto` when stdout is not a terminal or `NO_COLOR` is set.
pub(crate) fn human_stdout(color: ColorMode) -> anstream::AutoStream<std::io::Stdout> {
    anstream::AutoStream::new(std::io::stdout(), color.into())
}

/// How a piece of the human view is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Muted,
    Info,
    Job,
    Model,
    Tool,
    Good,
    Warn,
    Bad,
    Command,
}

impl Tone {
    pub(crate) fn style(self) -> Style {
        let fg = |c: AnsiColor| Style::new().fg_color(Some(c.into()));
        match self {
            Self::Plain => Style::new(),
            Self::Muted => fg(AnsiColor::BrightBlack),
            Self::Info => fg(AnsiColor::BrightCyan),
            Self::Job => fg(AnsiColor::Cyan),
            Self::Model => fg(AnsiColor::BrightMagenta),
            Self::Tool => fg(AnsiColor::BrightYellow),
            Self::Good => fg(AnsiColor::BrightGreen),
            Self::Warn => fg(AnsiColor::Yellow),
            Self::Bad => fg(AnsiColor::BrightRed),
            Self::Command => Style::new().bold(),
        }
    }
}

/// Untrusted text made safe to print: control characters dropped (line
/// breaks and tabs become spaces) and cut to `max` characters.
pub(crate) fn sanitize(text: &str, max: usize) -> String {
    let mut clean = String::with_capacity(text.len().min(max));
    let mut count = 0;
    for c in text.chars() {
        let c = match c {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if count == max {
            clean.truncate(clean.trim_end().len());
            clean.push_str("...");
            return clean;
        }
        clean.push(c);
        count += 1;
    }
    clean
}

/// Rows laid out in columns two spaces apart, the header first. Cells go
/// through `sanitize`.
pub(crate) fn table(header: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let header: Vec<String> = header.iter().map(|h| (*h).to_owned()).collect();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| sanitize(cell, MAX_WIDTH)).collect())
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (at, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(at) {
                Some(w) => *w = (*w).max(width),
                None => widths.push(width),
            }
        }
    }
    std::iter::once(&header)
        .chain(&rows)
        .map(|row| {
            let mut line = String::new();
            for (at, cell) in row.iter().enumerate() {
                let width = widths.get(at).copied().unwrap_or_default();
                line.push_str(&format!("{cell:<width$}  "));
            }
            line.truncate(line.trim_end().len());
            line
        })
        .collect()
}

/// Writes plain lines, flushing after the last.
pub(crate) fn write_plain(out: &mut impl Write, lines: &[String]) -> std::io::Result<()> {
    for line in lines {
        writeln!(out, "{line}")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_human_only_on_a_terminal_unless_a_flag_says() {
        assert_eq!(resolve_output(None, true), Output::Human);
        assert_eq!(resolve_output(None, false), Output::Json);
        assert_eq!(resolve_output(Some(Output::Json), true), Output::Json);
        assert_eq!(resolve_output(Some(Output::Human), false), Output::Human);
    }

    #[test]
    fn sanitize_drops_control_characters_and_cuts() {
        assert_eq!(sanitize("a\u{1b}[31mred\u{7}", 50), "a[31mred");
        assert_eq!(sanitize("two\nlines\tand\r", 50), "two lines and ");
        assert_eq!(sanitize("abcdef", 3), "abc...");
        assert_eq!(sanitize("ab def", 3), "ab...");
        assert_eq!(sanitize("abc", 3), "abc");
        assert_eq!(sanitize("\u{9b}31m", 10), "31m");
    }

    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["req-mass".to_owned(), "must".to_owned()],
            vec!["r".to_owned(), "should".to_owned()],
        ];
        assert_eq!(
            table(&["requirement", "priority"], &rows),
            vec![
                "requirement  priority",
                "req-mass     must",
                "r            should"
            ]
        );
    }
}
