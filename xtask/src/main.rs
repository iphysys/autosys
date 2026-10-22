//! Developer automation. Run with `cargo xtask <task>`.
//!
//! CI calls only `cargo xtask ci`, so the same checks run locally and in CI.
//! `ci` needs a Postgres server at `DATABASE_URL` (see README, section
//! Database).

mod arch_check;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  ci            Format check, clippy (warnings as errors), arch-check, database
                migrations and sqlx query data, tests, cargo-deny
  arch-check    Allowed crate dependencies (blueprint 10.3) and the SDR single-writer rule (7.2)";

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
        Some("arch-check") => arch_check(&workspace_root()?),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task `{other}`\n\n{USAGE}"),
        None => bail!("no task given\n\n{USAGE}"),
    }
}

/// A named check: either a `cargo` command or the in-process arch-check.
enum Step {
    Cargo {
        name: &'static str,
        args: &'static [&'static str],
    },
    ArchCheck,
}

/// Workspace-relative directory of the SQL migrations.
const MIGRATIONS: &str = "crates/infra/migrations";

const CI_STEPS: &[Step] = &[
    Step::Cargo {
        name: "format",
        args: &["fmt", "--all", "--check"],
    },
    Step::Cargo {
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
    Step::ArchCheck,
    // Creates the database at DATABASE_URL if needed and applies the
    // migrations, so the query-data check below describes the current schema.
    Step::Cargo {
        name: "database",
        args: &["sqlx", "database", "setup", "--source", MIGRATIONS],
    },
    // Fails if `.sqlx/` no longer matches the queries or the schema.
    Step::Cargo {
        name: "sqlx query data",
        args: &["sqlx", "prepare", "--workspace", "--check"],
    },
    Step::Cargo {
        name: "tests",
        args: &["test", "--workspace", "--all-features", "--locked"],
    },
    Step::Cargo {
        name: "cargo-deny",
        args: &["deny", "--locked", "check"],
    },
];

fn ci() -> Result<()> {
    let root = workspace_root()?;
    ensure_tool(&root, "deny", "cargo install --locked cargo-deny")?;
    ensure_tool(
        &root,
        "sqlx",
        "cargo install --locked sqlx-cli --version 0.9.0 --no-default-features --features postgres",
    )?;
    ensure_database_url()?;
    for step in CI_STEPS {
        match step {
            Step::Cargo { name, args } => {
                eprintln!("xtask: ci: running {name} (cargo {})", args.join(" "));
                // Query macros read the committed `.sqlx/` data, never the
                // live database, so CI proves that data is complete.
                // `cargo sqlx prepare` overrides this for its own build.
                let status = cargo(&root)
                    .env("SQLX_OFFLINE", "true")
                    .args(*args)
                    .status()
                    .with_context(|| format!("failed to start step `{name}`"))?;
                if !status.success() {
                    bail!("ci step `{name}` failed ({status})");
                }
            }
            Step::ArchCheck => {
                eprintln!("xtask: ci: running arch-check");
                arch_check(&root).context("ci step `arch-check` failed")?;
            }
        }
    }
    eprintln!("xtask: ci: all steps passed");
    Ok(())
}

fn arch_check(root: &Path) -> Result<()> {
    let output = cargo(root)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata = serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
    arch_check::run(root, &metadata)?;
    eprintln!("xtask: arch-check: passed");
    Ok(())
}

/// Fails early with install instructions instead of a confusing cargo error.
fn ensure_tool(root: &Path, subcommand: &str, install: &str) -> Result<()> {
    let installed = cargo(root)
        .args([subcommand, "--version"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !installed {
        bail!("`cargo {subcommand}` is not installed; install it with `{install}`");
    }
    Ok(())
}

/// The database steps and tests need a Postgres server. Never prints the
/// URL, which carries a password.
fn ensure_database_url() -> Result<()> {
    if std::env::var_os("DATABASE_URL").is_none_or(|url| url.is_empty()) {
        bail!(
            "DATABASE_URL is not set; start Postgres with \
             `docker compose --env-file .env -f deploy/compose/compose.yaml up -d --wait` \
             and set DATABASE_URL (see README, section Database)"
        );
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
