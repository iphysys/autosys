//! Developer automation. Run with `cargo xtask <task>`.
//!
//! CI calls only `cargo xtask ci`, so the same checks run locally and in CI.
//! `ci` needs a Postgres server at `DATABASE_URL` (see README, section
//! Database), uv for the generated-code check, and Docker for the tool
//! servers (see README, section Tool servers).

mod arch_check;
mod codegen;
mod tool_servers;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task>

Tasks:
  ci            Format check, clippy (warnings as errors), arch-check, config, benchmarks,
                generated code, database migrations and sqlx query data, tests, tool
                servers, cargo-deny
  arch-check    Allowed crate dependencies (blueprint 10.3) and the SDR single-writer rule (7.2)
  validate-config
                Every file in config/ against its schema (blueprint 11.2), and the model
                routing checks of the gateway: unique profiles, headers, agent profiles
                and tool calling (ADR-0027)
  validate-benchmarks
                Every problem in evals/benchmarks/ against its schema (blueprint 14.7)
  codegen [--check]
                Python models for the tool servers from schemas/tools/ (blueprint 7.1);
                --check fails if the committed models differ
  test-tools    Each tool server's tests in its container, and its sandbox: internal network
                without internet egress, resource limits (blueprint 17)";

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
        Some("validate-config") => validate_config(&workspace_root()?),
        Some("codegen") => {
            let check = match std::env::args().nth(2).as_deref() {
                None => false,
                Some("--check") => true,
                Some(other) => bail!("unknown codegen option `{other}`\n\n{USAGE}"),
            };
            codegen::run(&workspace_root()?, check)
        }
        Some("validate-benchmarks") => validate_benchmarks(&workspace_root()?),
        Some("test-tools") => tool_servers::run(&workspace_root()?),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task `{other}`\n\n{USAGE}"),
        None => bail!("no task given\n\n{USAGE}"),
    }
}

/// A named check: either a `cargo` command or one of the in-process tasks.
enum Step {
    Cargo {
        name: &'static str,
        args: &'static [&'static str],
    },
    ArchCheck,
    ValidateConfig,
    ValidateBenchmarks,
    CodegenCheck,
    TestTools,
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
    Step::ValidateConfig,
    Step::ValidateBenchmarks,
    Step::CodegenCheck,
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
    Step::TestTools,
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
            Step::ValidateConfig => {
                eprintln!("xtask: ci: running validate-config");
                validate_config(&root).context("ci step `validate-config` failed")?;
            }
            Step::ValidateBenchmarks => {
                eprintln!("xtask: ci: running validate-benchmarks");
                validate_benchmarks(&root).context("ci step `validate-benchmarks` failed")?;
            }
            Step::CodegenCheck => {
                eprintln!("xtask: ci: running codegen --check");
                codegen::run(&root, true).context("ci step `codegen --check` failed")?;
            }
            Step::TestTools => {
                eprintln!("xtask: ci: running test-tools");
                tool_servers::run(&root).context("ci step `test-tools` failed")?;
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

/// Validates `config/` with the same loader the harness uses, then the model
/// routing with the gateway's own checks, including every agent's profile.
fn validate_config(root: &Path) -> Result<()> {
    let config = root.join("config");
    let loader = infra::config::ConfigLoader::new().context("loading the config schemas")?;
    let mut problems = loader.validate_dir(&config).context("reading config/")?;
    if problems.is_empty() {
        problems.extend(model_routing_problems(&loader, &config));
    }
    if !problems.is_empty() {
        bail!("invalid configuration:\n  - {}", problems.join("\n  - "));
    }
    eprintln!("xtask: validate-config: passed");
    Ok(())
}

/// The checks `ModelGateway::start` makes that need no environment: the
/// routing is consistent, and every agent in `config/agents/` has a profile
/// that exists and, if the agent has tools, supports tool calling.
fn model_routing_problems(loader: &infra::config::ConfigLoader, config: &Path) -> Vec<String> {
    let routing_path = config.join("models/routing.yaml");
    // The providers the harness ships with check each profile's provider and
    // settings (ADR-0028).
    let routing = match model_gateway::providers::default_registry().and_then(|registry| {
        loader
            .load::<contracts::types::ModelRouting>(&routing_path)
            .and_then(|config| model_gateway::Routing::new(config, std::sync::Arc::new(registry)))
    }) {
        Ok(routing) => routing,
        Err(e) => return vec![format!("{}: {}", routing_path.display(), e.message())],
    };
    let agents_dir = config.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| {
            loader
                .load::<contracts::types::AgentSpec>(path)
                .and_then(|agent| routing.check_agent(&agent))
                .err()
                .map(|e| format!("{}: {}", path.display(), e.message()))
        })
        .collect()
}

/// Validates `evals/benchmarks/` with the loader the evaluation runs use.
/// Approval outcomes must name rules of the autonomy policy.
fn validate_benchmarks(root: &Path) -> Result<()> {
    let config = infra::config::ConfigLoader::new().context("loading the config schemas")?;
    let policy: contracts::types::AutonomyPolicy = config
        .load(&root.join("config/policies/autonomy.yaml"))
        .context("loading the autonomy policy")?;
    let rules: BTreeSet<String> = policy.approvals.iter().map(|r| r.id.to_string()).collect();
    let loader =
        infra::benchmarks::BenchmarkLoader::new().context("loading the benchmark schema")?;
    let problems = loader
        .validate_dir(&root.join("evals/benchmarks"), &rules)
        .context("reading evals/benchmarks/")?;
    if !problems.is_empty() {
        bail!("invalid benchmarks:\n  - {}", problems.join("\n  - "));
    }
    eprintln!("xtask: validate-benchmarks: passed");
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
