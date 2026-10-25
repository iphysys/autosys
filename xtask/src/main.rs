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
                Every file in config/ against its schema (blueprint 11.2), the model
                routing checks of the gateway: unique profiles, headers, agent profiles
                and tool calling (ADR-0027), the context profiles against their ceiling,
                every prompt header in prompts/ (ADR-0029), and every agent spec through
                the runtime's agent registry: prompt, tools, skills, result schema and
                repair behavior (ADR-0030)
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

/// The environment `SYNTH_ENVIRONMENT` names; unset is production, which is
/// what the committed `config/` is.
fn environment() -> Result<contracts::Environment> {
    let value = std::env::var(contracts::Environment::VAR).ok();
    contracts::Environment::from_var(value.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Validates `config/` with the same loader the harness uses, for the
/// environment `SYNTH_ENVIRONMENT` names, then the model routing with the
/// gateway's own checks, including every agent's profile.
fn validate_config(root: &Path) -> Result<()> {
    let config = root.join("config");
    let loader =
        infra::config::ConfigLoader::new(environment()?).context("loading the config schemas")?;
    let mut problems = loader.validate_dir(&config).context("reading config/")?;
    if problems.is_empty() {
        problems.extend(model_routing_problems(&loader, &config));
        problems.extend(context_problems(&loader, root));
        problems.extend(agent_problems(&loader, root));
    }
    if !problems.is_empty() {
        bail!("invalid configuration:\n  - {}", problems.join("\n  - "));
    }
    eprintln!("xtask: validate-config: passed");
    Ok(())
}

/// The checks of the context manager that a schema cannot make: every
/// profile in `config/context.yaml` fits its own total and the ceiling in
/// `config/policies/context.yaml`, a `default` profile exists, and every
/// prompt under `prompts/` has a valid header and a unique id (ADR-0029).
fn context_problems(loader: &infra::config::ConfigLoader, root: &Path) -> Vec<String> {
    let config = root.join("config");
    let mut problems = Vec::new();
    let loaded = loader
        .load::<contracts::types::ContextBudgets>(&config.join("context.yaml"))
        .and_then(|budgets| {
            loader
                .load::<contracts::types::ContextPolicy>(&config.join("policies/context.yaml"))
                .map(|policy| (budgets, policy))
        });
    match loaded {
        Ok((budgets, policy)) => {
            if let Err(e) = context::ContextProfiles::new(budgets, &policy) {
                problems.push(format!("config/context.yaml: {e}"));
            }
        }
        Err(e) => problems.push(e.message().to_owned()),
    }
    if let Err(e) = context::PromptLibrary::load_dir(&root.join("prompts")) {
        problems.push(format!("prompts/: {e}"));
    }
    problems
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
            .and_then(|config| {
                model_gateway::Routing::new(
                    config,
                    std::sync::Arc::new(registry),
                    loader.environment(),
                )
            })
    }) {
        Ok(routing) => routing,
        Err(e) => return vec![format!("{}: {}", routing_path.display(), e.message())],
    };
    yaml_files(&config.join("agents"))
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

/// Every agent in `config/agents/` through the registry the runtime builds
/// at startup: its prompt is in `prompts/`, its tools are the SDR tools or
/// listed in `config/tools/`, it names no skill, its result schema compiles,
/// and it does not escalate (ADR-0029 follow-up 5, ADR-0030).
fn agent_problems(loader: &infra::config::ConfigLoader, root: &Path) -> Vec<String> {
    let config = root.join("config");
    let specs: Vec<contracts::types::AgentSpec> = match load_all(loader, &config.join("agents")) {
        Ok(specs) => specs,
        Err(problem) => return vec![problem],
    };
    let overlays: Vec<contracts::types::ToolOverlay> = match load_all(loader, &config.join("tools"))
    {
        Ok(overlays) => overlays,
        Err(problem) => return vec![problem],
    };
    let tools: Vec<&str> = overlays
        .iter()
        .flat_map(|o| o.tools.iter().map(|t| t.name.as_str()))
        .collect();
    let prompts = match context::PromptLibrary::load_dir(&root.join("prompts")) {
        Ok(prompts) => prompts,
        Err(e) => return vec![format!("prompts/: {e}")],
    };
    match runtime::AgentRegistry::new(specs, &prompts, &tools) {
        Ok(_) => Vec::new(),
        Err(runtime::RuntimeError::InvalidAgents { problems }) => problems
            .into_iter()
            .map(|p| format!("config/agents/: {p}"))
            .collect(),
        Err(e) => vec![format!("config/agents/: {e}")],
    }
}

/// Every `*.yaml` file directly in `dir`, loaded as `T`.
fn load_all<T: infra::config::ConfigFile>(
    loader: &infra::config::ConfigLoader,
    dir: &Path,
) -> Result<Vec<T>, String> {
    yaml_files(dir)
        .iter()
        .map(|path| loader.load(path).map_err(|e| e.message().to_owned()))
        .collect()
}

/// The `*.yaml` files directly in `dir`, sorted; none if it does not exist.
fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    paths.sort();
    paths
}

/// Validates `evals/benchmarks/` with the loader the evaluation runs use.
/// Approval outcomes must name rules of the autonomy policy.
fn validate_benchmarks(root: &Path) -> Result<()> {
    let config =
        infra::config::ConfigLoader::new(environment()?).context("loading the config schemas")?;
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn loader() -> infra::config::ConfigLoader {
        infra::config::ConfigLoader::new(contracts::Environment::Production)
            .expect("config schemas compile")
    }

    #[test]
    fn the_repository_passes_the_context_checks() {
        let problems = context_problems(&loader(), &workspace_root().expect("root"));
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// The committed config and prompts in a temporary root, to break one file at a time.
    fn copy_of_the_repository() -> tempfile::TempDir {
        let root = workspace_root().expect("root");
        let dir = tempfile::tempdir().expect("temp dir");
        for file in [
            "config/agents/main_orchestrator.yaml",
            "config/context.yaml",
            "config/policies/context.yaml",
            "config/tools/build123d.yaml",
            "prompts/system/base.md",
            "prompts/orchestrator/main.md",
        ] {
            let target = dir.path().join(file);
            fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
            fs::copy(root.join(file), target).expect("copy");
        }
        dir
    }

    #[test]
    fn a_profile_above_the_ceiling_is_a_problem() {
        let dir = copy_of_the_repository();
        fs::write(
            dir.path().join("config/policies/context.yaml"),
            "schema_version: \"0.1\"\nmax_total_tokens: 20000\n",
        )
        .expect("write");
        let problems = context_problems(&loader(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("ceiling 20000"), "{problems:#?}");
    }

    #[test]
    fn a_broken_prompt_header_is_a_problem() {
        let dir = copy_of_the_repository();
        let path = dir.path().join("prompts/orchestrator/main.md");
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("  - run_history\n"), "the fixture changed");
        fs::write(
            &path,
            text.replace("  - run_history\n", "  - allowed_capabilities\n"),
        )
        .expect("write");
        let problems = context_problems(&loader(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("allowed_capabilities"),
            "{problems:#?}"
        );
    }

    #[test]
    fn the_repository_passes_the_agent_checks() {
        let problems = agent_problems(&loader(), &workspace_root().expect("root"));
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn a_broken_agent_prompt_is_a_problem() {
        let dir = copy_of_the_repository();
        assert!(agent_problems(&loader(), dir.path()).is_empty());
        let path = dir.path().join("config/agents/main_orchestrator.yaml");
        // The edit matches whole lines ending in "\n"; a Windows checkout with
        // core.autocrlf has CRLF, which would leave the text unedited.
        let text = fs::read_to_string(&path)
            .expect("read")
            .replace("\r\n", "\n");
        assert!(
            text.contains("prompt: orchestrator/main.md\n"),
            "the fixture changed"
        );
        fs::write(
            &path,
            text.replace(
                "prompt: orchestrator/main.md\n",
                "prompt: orchestrator/mian.md\n",
            ),
        )
        .expect("write");
        let problems = agent_problems(&loader(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].starts_with("config/agents/: agent `main_orchestrator`: prompt:"),
            "{problems:#?}"
        );
        assert!(
            problems[0].contains("orchestrator/mian.md"),
            "{problems:#?}"
        );
        assert!(
            problems[0].contains("available: orchestrator/main.md, system/base.md"),
            "{problems:#?}"
        );
    }

    #[test]
    fn an_agent_tool_that_no_overlay_lists_is_a_problem() {
        let dir = copy_of_the_repository();
        fs::remove_file(dir.path().join("config/tools/build123d.yaml")).expect("remove");
        let problems = agent_problems(&loader(), dir.path());
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(
            problems[0].contains("tool `create_plate` is not available"),
            "{problems:#?}"
        );
        assert!(
            problems[1].contains("tool `measure` is not available"),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_missing_context_file_is_a_problem() {
        let dir = copy_of_the_repository();
        fs::remove_file(dir.path().join("config/context.yaml")).expect("remove");
        let problems = context_problems(&loader(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("context.yaml"), "{problems:#?}");
    }
}
