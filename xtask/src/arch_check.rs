//! `cargo xtask arch-check`: enforces the allowed dependencies between
//! workspace crates (blueprint sections 3.3 and 10.3), the SDR single-writer
//! rule (section 7.2), and who may issue lease fences, grant write authority
//! and call tools (ADR-0032).

use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Allowed workspace dependencies per crate, copied from blueprint section
/// 10.3. A crate missing from this table fails the check until it is added
/// here, and to the blueprint if the edge is new.
const ALLOWED: &[(&str, &[&str])] = &[
    ("synth-schema-types", &[]),
    ("synth-contracts", &["synth-schema-types"]),
    ("synth-sdr", &["synth-contracts"]),
    ("synth-process", &["synth-contracts", "synth-sdr"]),
    ("synth-knowledge", &["synth-contracts"]),
    (
        "synth-context",
        &[
            "synth-contracts",
            "synth-sdr",
            "synth-process",
            "synth-knowledge",
        ],
    ),
    ("synth-governance", &["synth-contracts"]),
    ("synth-tools", &["synth-contracts", "synth-governance"]),
    (
        "synth-evaluation",
        &["synth-contracts", "synth-sdr", "synth-tools"],
    ),
    ("synth-artifacts", &["synth-contracts", "synth-sdr"]),
    ("synth-model-gateway", &["synth-contracts"]),
    (
        "synth-runtime",
        &[
            "synth-contracts",
            "synth-sdr",
            "synth-process",
            "synth-context",
            "synth-tools",
            "synth-evaluation",
            "synth-governance",
            "synth-artifacts",
            "synth-model-gateway",
        ],
    ),
    (
        "synth-improvement",
        &["synth-contracts", "synth-runtime", "synth-evaluation"],
    ),
    ("synth-infra", &["synth-contracts", "synth-schema-types"]),
    ("synth-api", &["synth-contracts", "synth-runtime"]),
    ("synth-testkit", &["synth-contracts", "synth-schema-types"]),
];

/// The composition root may depend on every crate above except testkit.
const BOOTSTRAP: &str = "synth-bootstrap";
/// Binaries under `apps/` may depend on these only.
const APPS_ALLOWED: &[&str] = &[BOOTSTRAP, "synth-api"];
/// Test support: allowed only as a dev-dependency, by any crate.
const TESTKIT: &str = "synth-testkit";
/// Developer automation, outside the product dependency graph.
const EXEMPT: &[&str] = &["xtask"];

/// Crate directories under `crates/` that may name the SDR repository trait.
const SDR_REPOSITORY_USERS: &[&str] = &["contracts", "testkit", "sdr", "infra", "bootstrap"];
const SDR_REPOSITORY: &str = "SdrRepository";

/// Where write authority may be granted: the scheduler, through bootstrap,
/// plus the contract itself and test support. `bootstrap` and the scheduler
/// do not exist yet (PR 12).
const AUTHORITY_GRANTERS: &[&str] = &[
    "crates/contracts",
    "crates/testkit",
    "crates/bootstrap",
    "crates/runtime/src/scheduler",
];

/// A name that may appear only under the listed paths (ADR-0032). Like the
/// SDR rule, these are text rules: an alias defeats them, so review must
/// catch one.
pub struct NameRule {
    pub name: &'static str,
    /// Workspace-relative directories or files, compared by path component.
    pub allowed: &'static [&'static str],
    pub why: &'static str,
}

pub const NAME_RULES: &[NameRule] = &[
    NameRule {
        name: "issue_for_claim",
        allowed: &["crates/contracts", "crates/testkit", "crates/infra"],
        why: "only JobStore::claim implementations issue fence tokens",
    },
    NameRule {
        name: "authorized_by",
        allowed: AUTHORITY_GRANTERS,
        why: "only the scheduler, through bootstrap, gives a context its write authority",
    },
    NameRule {
        name: "UnfencedReason",
        allowed: AUTHORITY_GRANTERS,
        why: "unfenced write authority is a capability; tests get it from testkit::leases",
    },
    NameRule {
        name: ".invoke(",
        allowed: &["crates/tools/src/gateway.rs", "crates/testkit"],
        why: "only ToolGateway calls Tool::invoke, after the fenced tool_call_started",
    },
];

/// Runs every check and fails with every violation found.
pub fn run(root: &Path, metadata: &Value) -> Result<()> {
    let mut violations = dependency_violations(metadata)?;
    let files = source_files(root)?;
    violations.extend(sdr_repository_violations(&files));
    violations.extend(name_violations(&files, NAME_RULES));
    if !violations.is_empty() {
        bail!(
            "architecture rules violated:\n  - {}",
            violations.join("\n  - ")
        );
    }
    Ok(())
}

fn allowed_for(package: &str, is_app: bool) -> Option<Vec<&'static str>> {
    if is_app {
        return Some(APPS_ALLOWED.to_vec());
    }
    if package == BOOTSTRAP {
        return Some(
            ALLOWED
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| *name != TESTKIT)
                .collect(),
        );
    }
    ALLOWED
        .iter()
        .find(|(name, _)| *name == package)
        .map(|(_, deps)| deps.to_vec())
}

/// Checks `cargo metadata --no-deps` output: every dependency between
/// workspace packages must be allowed.
pub fn dependency_violations(metadata: &Value) -> Result<Vec<String>> {
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .context("cargo metadata has no packages array")?;
    let workspace: Vec<&str> = packages
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .collect();

    let mut violations = Vec::new();
    for package in packages {
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .context("package without a name")?;
        if EXEMPT.contains(&name) {
            continue;
        }
        let is_app = package
            .get("manifest_path")
            .and_then(Value::as_str)
            .is_some_and(|path| {
                Path::new(path)
                    .components()
                    .any(|c| c == Component::Normal("apps".as_ref()))
            });
        let Some(allowed) = allowed_for(name, is_app) else {
            violations.push(format!(
                "{name} is not in the blueprint section 10.3 table; add it to xtask/src/arch_check.rs"
            ));
            continue;
        };
        let dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for dependency in dependencies {
            let Some(dep) = dependency.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !workspace.contains(&dep) {
                continue;
            }
            let kind = dependency
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("normal");
            if dep == TESTKIT {
                if kind != "dev" {
                    violations.push(format!(
                        "{name} depends on {TESTKIT} as a {kind} dependency; it may only be a dev-dependency"
                    ));
                }
            } else if !allowed.contains(&dep) {
                violations.push(format!(
                    "{name} -> {dep} ({kind}) is not allowed by blueprint section 10.3"
                ));
            }
        }
    }
    Ok(violations)
}

/// A Rust source file: the crate directory it belongs to, its path, and its text.
pub struct SourceFile {
    pub crate_dir: String,
    pub under_apps: bool,
    pub path: String,
    pub text: String,
}

/// Every `.rs` file under `crates/*/` and `apps/*/`.
fn source_files(root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    for (top, under_apps) in [("crates", false), ("apps", true)] {
        let dir = root.join(top);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let crate_path = entry?.path();
            if !crate_path.is_dir() {
                continue;
            }
            let crate_dir = crate_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut paths = Vec::new();
            collect_rust_files(&crate_path, &mut paths)?;
            for path in paths {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                files.push(SourceFile {
                    crate_dir: crate_dir.clone(),
                    under_apps,
                    path: path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                    text,
                });
            }
        }
    }
    Ok(files)
}

fn collect_rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                collect_rust_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Only `SdrManager`'s crate, its storage adapter, the composition root, the
/// trait's own crate and testkit may name the SDR repository (blueprint
/// section 7.2). Comment lines are ignored.
pub fn sdr_repository_violations(files: &[SourceFile]) -> Vec<String> {
    files
        .iter()
        .filter(|f| f.under_apps || !SDR_REPOSITORY_USERS.contains(&f.crate_dir.as_str()))
        .flat_map(|f| {
            f.text
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim_start().starts_with("//"))
                .filter(|(_, line)| line.contains(SDR_REPOSITORY))
                .map(move |(n, _)| {
                    format!(
                        "{}:{} names {SDR_REPOSITORY}; only SdrManager may use it (blueprint section 7.2)",
                        f.path,
                        n + 1
                    )
                })
        })
        .collect()
}

/// Whether `path` is `prefix` or lies under it. Both are split on slashes
/// and backslashes, so a Windows path matches a rule written with `/`.
fn is_under(path: &str, prefix: &str) -> bool {
    let parts = |p: &str| -> Vec<String> {
        p.split(['/', '\\'])
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_owned)
            .collect()
    };
    let (path, prefix) = (parts(path), parts(prefix));
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// Lines outside a rule's allowed paths that name its name. Comment lines
/// are ignored.
pub fn name_violations(files: &[SourceFile], rules: &[NameRule]) -> Vec<String> {
    let mut violations = Vec::new();
    for rule in rules {
        for file in files {
            if rule.allowed.iter().any(|a| is_under(&file.path, a)) {
                continue;
            }
            for (n, line) in file.text.lines().enumerate() {
                if !line.trim_start().starts_with("//") && line.contains(rule.name) {
                    violations.push(format!(
                        "{}:{} names {}; {} (ADR-0032)",
                        file.path,
                        n + 1,
                        rule.name,
                        rule.why
                    ));
                }
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn package(name: &str, manifest: &str, deps: &[(&str, Option<&str>)]) -> Value {
        let dependencies: Vec<Value> = deps
            .iter()
            .map(|(dep, kind)| json!({ "name": dep, "kind": kind }))
            .collect();
        json!({ "name": name, "manifest_path": manifest, "dependencies": dependencies })
    }

    fn metadata(packages: Vec<Value>) -> Value {
        json!({ "packages": packages })
    }

    fn base() -> Vec<Value> {
        vec![
            package(
                "synth-schema-types",
                "/r/crates/schema-types/Cargo.toml",
                &[("serde", None)],
            ),
            package(
                "synth-contracts",
                "/r/crates/contracts/Cargo.toml",
                &[("synth-schema-types", None), ("tokio", Some("dev"))],
            ),
            package(
                "synth-testkit",
                "/r/crates/testkit/Cargo.toml",
                &[("synth-contracts", None), ("synth-schema-types", None)],
            ),
            package("xtask", "/r/xtask/Cargo.toml", &[("anyhow", None)]),
        ]
    }

    #[test]
    fn allowed_edges_pass() {
        let violations = dependency_violations(&metadata(base())).expect("valid metadata");
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn forbidden_edge_fails() {
        let mut packages = base();
        packages.push(package(
            "synth-sdr",
            "/r/crates/sdr/Cargo.toml",
            &[("synth-contracts", None), ("synth-schema-types", None)],
        ));
        let violations = dependency_violations(&metadata(packages)).expect("valid metadata");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("synth-sdr -> synth-schema-types"));
    }

    #[test]
    fn testkit_only_as_dev_dependency() {
        let mut packages = base();
        packages.push(package(
            "synth-sdr",
            "/r/crates/sdr/Cargo.toml",
            &[("synth-contracts", None), ("synth-testkit", Some("dev"))],
        ));
        packages.push(package(
            "synth-governance",
            "/r/crates/governance/Cargo.toml",
            &[("synth-contracts", None), ("synth-testkit", None)],
        ));
        let violations = dependency_violations(&metadata(packages)).expect("valid metadata");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("synth-governance depends on synth-testkit"));
    }

    #[test]
    fn unknown_crate_fails() {
        let mut packages = base();
        packages.push(package("synth-misc", "/r/crates/misc/Cargo.toml", &[]));
        let violations = dependency_violations(&metadata(packages)).expect("valid metadata");
        assert!(
            violations[0].contains("synth-misc is not in"),
            "{violations:?}"
        );
    }

    #[test]
    fn apps_use_bootstrap_and_api_only() {
        let mut packages = base();
        packages.push(package(
            "synth-worker",
            "/r/apps/worker/Cargo.toml",
            &[("synth-bootstrap", None), ("synth-contracts", None)],
        ));
        let violations = dependency_violations(&metadata(packages)).expect("valid metadata");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("synth-worker -> synth-contracts"));
    }

    #[test]
    fn bootstrap_may_use_everything_but_testkit() {
        let allowed = allowed_for(BOOTSTRAP, false).expect("bootstrap is known");
        assert!(allowed.contains(&"synth-runtime"));
        assert!(allowed.contains(&"synth-infra"));
        assert!(!allowed.contains(&TESTKIT));
    }

    fn file(crate_dir: &str, under_apps: bool, text: &str) -> SourceFile {
        SourceFile {
            crate_dir: crate_dir.to_owned(),
            under_apps,
            path: format!("{crate_dir}/src/lib.rs"),
            text: text.to_owned(),
        }
    }

    #[test]
    fn sdr_repository_only_in_allowed_crates() {
        let files = [
            file("sdr", false, "use contracts::SdrRepository;"),
            file("infra", false, "impl SdrRepository for PgSdr {}"),
            file(
                "runtime",
                false,
                "// mentions SdrRepository in a comment\nfn main() {}",
            ),
            file(
                "runtime",
                false,
                "fn f(_: &dyn contracts::SdrRepository) {}",
            ),
            file("worker", true, "use contracts::SdrRepository;"),
        ];
        let violations = sdr_repository_violations(&files);
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].starts_with("runtime/src/lib.rs:1"));
        assert!(violations[1].starts_with("worker/src/lib.rs:1"));
    }

    fn at(path: &str, text: &str) -> SourceFile {
        SourceFile {
            crate_dir: path
                .split(['/', '\\'])
                .nth(1)
                .unwrap_or_default()
                .to_owned(),
            under_apps: path.starts_with("apps"),
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    fn rule(name: &str) -> &'static NameRule {
        NAME_RULES
            .iter()
            .find(|r| r.name == name)
            .expect("the rule exists")
    }

    #[test]
    fn paths_match_by_component_on_every_os() {
        assert!(is_under(
            "crates/infra/src/persistence/jobs.rs",
            "crates/infra"
        ));
        assert!(is_under(
            "crates\\infra\\src\\persistence\\jobs.rs",
            "crates/infra"
        ));
        assert!(is_under(
            "crates\\tools\\src\\gateway.rs",
            "crates/tools/src/gateway.rs"
        ));
        assert!(!is_under(
            "crates/infrastructure/src/lib.rs",
            "crates/infra"
        ));
        assert!(!is_under(
            "crates/tools/src/gateway_extra.rs",
            "crates/tools/src/gateway.rs"
        ));
        assert!(!is_under("crates", "crates/infra"));
    }

    #[test]
    fn only_claim_implementations_issue_fences() {
        let files = [
            at(
                "crates/infra/src/persistence/jobs.rs",
                "FenceToken::issue_for_claim(id, h, 1)",
            ),
            at(
                "crates\\testkit\\src\\stores\\job.rs",
                "FenceToken::issue_for_claim(id, h, 1)",
            ),
            at(
                "crates/runtime/src/lib.rs",
                "// issue_for_claim in a comment",
            ),
            at(
                "crates/runtime/src/lib.rs",
                "let f = FenceToken::issue_for_claim(id, h, 9);",
            ),
            at(
                "crates/runtime/tests/support/mod.rs",
                "FenceToken::issue_for_claim(id, h, 9)",
            ),
        ];
        let violations = name_violations(&files, std::slice::from_ref(rule("issue_for_claim")));
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].starts_with("crates/runtime/src/lib.rs:1"));
        assert!(violations[1].starts_with("crates/runtime/tests/support/mod.rs:1"));
    }

    #[test]
    fn only_the_scheduler_grants_write_authority() {
        let files = [
            at(
                "crates/runtime/src/scheduler/claim.rs",
                ".authorized_by(authority)",
            ),
            at(
                "crates\\bootstrap\\src\\lib.rs",
                ".authorized_by(authority)",
            ),
            at(
                "crates/runtime/src/agents/bounded_worker.rs",
                ".authorized_by(authority)",
            ),
            at("crates/api/src/lib.rs", ".authorized_by(authority)"),
            at("apps/worker/src/main.rs", ".authorized_by(authority)"),
        ];
        let violations = name_violations(&files, std::slice::from_ref(rule("authorized_by")));
        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(violations[0].starts_with("crates/runtime/src/agents"));
        assert!(violations[1].starts_with("crates/api"));
        assert!(violations[2].starts_with("apps/worker"));
    }

    #[test]
    fn only_authority_granters_name_unfenced_reasons() {
        let files = [
            at("crates/contracts/src/fence.rs", "pub enum UnfencedReason {"),
            at(
                "crates/testkit/src/leases.rs",
                "WriteAuthority::Unfenced(UnfencedReason::JobInsert)",
            ),
            at(
                "crates/infra/src/persistence/jobs.rs",
                "UnfencedReason::JobInsert",
            ),
            at(
                "crates/infra/tests/claiming.rs",
                "UnfencedReason::JobInsert",
            ),
            at("crates/api/src/routes.rs", "UnfencedReason::ApiCommand"),
        ];
        let violations = name_violations(&files, std::slice::from_ref(rule("UnfencedReason")));
        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(
            violations
                .iter()
                .all(|v| !v.starts_with("crates/contracts"))
        );
        assert!(violations.iter().all(|v| !v.starts_with("crates/testkit")));
    }

    #[test]
    fn only_the_gateway_invokes_tools() {
        let files = [
            at(
                "crates\\tools\\src\\gateway.rs",
                "registered.tool.invoke(call.request, ctx)",
            ),
            at(
                "crates/testkit/src/contract/tool.rs",
                "tool.invoke(&valid_request, &ctx())",
            ),
            at(
                "crates/tools/src/mcp/client.rs",
                "self.inner.invoke(request, ctx)",
            ),
            at(
                "crates/runtime/tests/fencing.rs",
                "tool.invoke(&request, &ctx)",
            ),
            at("crates/tools/src/registry.rs", "async fn invoke(&self) {}"),
        ];
        let violations = name_violations(&files, std::slice::from_ref(rule(".invoke(")));
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].starts_with("crates/tools/src/mcp/client.rs:1"));
        assert!(violations[1].starts_with("crates/runtime/tests/fencing.rs:1"));
    }

    #[test]
    fn the_repository_passes_the_name_rules() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let files = source_files(&root).expect("read the sources");
        let violations = name_violations(&files, NAME_RULES);
        assert!(violations.is_empty(), "{violations:#?}");
    }
}
