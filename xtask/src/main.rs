//! Developer automation. Run with `cargo xtask <task>`.
//!
//! CI calls only `cargo xtask ci`, so the same checks run locally and in CI.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  ci    Format check, clippy (warnings as errors), tests, cargo-deny";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("ci") => ci(),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task `{other}`\n\n{USAGE}"),
        None => bail!("no task given\n\n{USAGE}"),
    }
}

/// A named check: the arguments passed to `cargo`.
struct Step {
    name: &'static str,
    args: &'static [&'static str],
}

const CI_STEPS: &[Step] = &[
    Step {
        name: "format",
        args: &["fmt", "--all", "--check"],
    },
    Step {
        name: "clippy",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    },
    Step {
        name: "tests",
        args: &["test", "--workspace", "--all-features", "--locked"],
    },
    Step {
        name: "cargo-deny",
        args: &["deny", "--locked", "check"],
    },
];

fn ci() -> Result<()> {
    let root = workspace_root()?;
    ensure_cargo_deny(&root)?;
    for step in CI_STEPS {
        eprintln!(
            "xtask: ci: running {} (cargo {})",
            step.name,
            step.args.join(" ")
        );
        let status = cargo(&root)
            .args(step.args)
            .status()
            .with_context(|| format!("failed to start step `{}`", step.name))?;
        if !status.success() {
            bail!("ci step `{}` failed ({status})", step.name);
        }
    }
    eprintln!("xtask: ci: all steps passed");
    Ok(())
}

/// Fails early with install instructions instead of a confusing cargo error.
fn ensure_cargo_deny(root: &Path) -> Result<()> {
    let installed = cargo(root)
        .args(["deny", "--version"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !installed {
        bail!("cargo-deny is not installed; install it with `cargo install --locked cargo-deny`");
    }
    Ok(())
}

/// Uses the cargo that launched xtask, so the pinned toolchain applies to every step.
fn cargo(root: &Path) -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root);
    cmd
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .context("xtask manifest directory has no parent")
}
