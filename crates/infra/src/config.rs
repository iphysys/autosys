//! Declarative configuration from `config/` (blueprint section 11).
//!
//! A file goes YAML → JSON value → its JSON Schema in `schemas/config/` →
//! typed value, so no file reaches the harness unvalidated. Errors name the
//! file and the JSON pointer of the problem.
//!
//! YAML is parsed by serde-saphyr: a pure-Rust parser with limits against
//! alias bombs and deep nesting. Booleans are YAML 1.2 (`no` stays a string),
//! duplicate keys are errors, and merge keys and custom tags are rejected.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use contracts::types::{
    AgentSpec, AutonomyPolicy, ContextBudgets, ContextPolicy, GatesPolicy, ModelRouting,
    RuntimeSettings, SCHEMA_FILES, ToolOverlay, WorkerSettings,
};
use contracts::{Environment, Error, ErrorCode, SchemaSet};
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy, Options};

/// A configuration object with a canonical schema.
pub trait ConfigFile: DeserializeOwned {
    /// The schema, relative to `schemas/`.
    const SCHEMA: &'static str;

    /// The rules of this section that its schema cannot state, for the
    /// environment the harness runs in: one message per problem, none if
    /// the section is valid. Runs after the schema check.
    fn validate(&self, _environment: Environment) -> Vec<String> {
        Vec::new()
    }
}

impl ConfigFile for AutonomyPolicy {
    const SCHEMA: &'static str = "config/autonomy-policy.schema.json";
}

impl ConfigFile for ContextBudgets {
    const SCHEMA: &'static str = "config/context-budgets.schema.json";
}

impl ConfigFile for ContextPolicy {
    const SCHEMA: &'static str = "config/context-policy.schema.json";
}

impl ConfigFile for GatesPolicy {
    const SCHEMA: &'static str = "config/gates-policy.schema.json";
}

impl ConfigFile for ToolOverlay {
    const SCHEMA: &'static str = "config/tool-overlay.schema.json";
}

impl ConfigFile for AgentSpec {
    const SCHEMA: &'static str = "config/agent.schema.json";
}

impl ConfigFile for ModelRouting {
    const SCHEMA: &'static str = "config/model-routing.schema.json";
}

impl ConfigFile for RuntimeSettings {
    const SCHEMA: &'static str = "config/runtime.schema.json";
}

/// Fastest intervals a worker may use outside the Test environment.
const MIN_LEASE_MS: u64 = 10_000;
const MIN_HEARTBEAT_MS: u64 = 1_000;
const MIN_POLL_INTERVAL_MS: u64 = 200;

impl ConfigFile for WorkerSettings {
    const SCHEMA: &'static str = "config/worker.schema.json";

    /// A heartbeat at most a third of the lease, so two renewals can fail
    /// before the lease runs out; polls more often than heartbeats; jitter
    /// smaller than the poll; a backoff that does not shrink; each tool
    /// server once. Outside Test, intervals no shorter than the floors.
    fn validate(&self, environment: Environment) -> Vec<String> {
        let lease = self.lease_ms.get();
        let heartbeat = self.heartbeat_ms.get();
        let poll = self.poll_interval_ms.get();
        let mut problems = Vec::new();
        if heartbeat.saturating_mul(3) > lease {
            problems.push(format!(
                "heartbeat_ms ({heartbeat}) must be at most a third of lease_ms ({lease})"
            ));
        }
        if poll >= heartbeat {
            problems.push(format!(
                "poll_interval_ms ({poll}) must be shorter than heartbeat_ms ({heartbeat})"
            ));
        }
        if self.poll_jitter_ms >= poll {
            problems.push(format!(
                "poll_jitter_ms ({}) must be smaller than poll_interval_ms ({poll})",
                self.poll_jitter_ms
            ));
        }
        if self.retry_backoff_max_ms < self.retry_backoff_min_ms {
            problems.push(format!(
                "retry_backoff_max_ms ({}) must be at least retry_backoff_min_ms ({})",
                self.retry_backoff_max_ms, self.retry_backoff_min_ms
            ));
        }
        let mut servers = BTreeSet::new();
        for server in &self.tool_servers {
            if !servers.insert(server.server_id.as_str()) {
                problems.push(format!(
                    "tool server {} is listed twice",
                    server.server_id.as_str()
                ));
            }
        }
        if environment != Environment::Test {
            for (field, value, floor) in [
                ("lease_ms", lease, MIN_LEASE_MS),
                ("heartbeat_ms", heartbeat, MIN_HEARTBEAT_MS),
                ("poll_interval_ms", poll, MIN_POLL_INTERVAL_MS),
            ] {
                if value < floor {
                    problems.push(format!(
                        "{field} ({value}) is below {floor}, allowed only in the test environment"
                    ));
                }
            }
        }
        problems
    }
}

/// Validates and loads configuration files, for one environment.
pub struct ConfigLoader {
    schemas: SchemaSet,
    environment: Environment,
}

impl ConfigLoader {
    /// A loader that checks each section's rules for `environment`.
    pub fn new(environment: Environment) -> Result<Self, Error> {
        Ok(Self {
            environment,
            schemas: SchemaSet::compile(&[
                AutonomyPolicy::SCHEMA,
                ContextBudgets::SCHEMA,
                ContextPolicy::SCHEMA,
                GatesPolicy::SCHEMA,
                ToolOverlay::SCHEMA,
                AgentSpec::SCHEMA,
                ModelRouting::SCHEMA,
                RuntimeSettings::SCHEMA,
                WorkerSettings::SCHEMA,
            ])?,
        })
    }

    /// The environment whose rules this loader applies.
    pub fn environment(&self) -> Environment {
        self.environment
    }

    /// Reads and validates the file at `path`.
    pub fn load<T: ConfigFile>(&self, path: &Path) -> Result<T, Error> {
        let origin = path.display().to_string();
        self.parse(&read_text(path)?, &origin)
    }

    /// Validates YAML text against its schema and its section's rules.
    /// `origin` names it in errors, usually its path.
    pub fn parse<T: ConfigFile>(&self, text: &str, origin: &str) -> Result<T, Error> {
        let value = yaml_value(text, origin)?;
        let problems = self.schemas.errors(T::SCHEMA, &value)?;
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "{origin}: does not satisfy schemas/{}: {}",
                    T::SCHEMA,
                    problems.join("; ")
                ),
            ));
        }
        let typed: T = serde_json::from_value(value).map_err(|e| {
            Error::new(
                ErrorCode::InvalidInput,
                format!("{origin}: does not match the configuration type: {e}"),
            )
        })?;
        let problems = typed.validate(self.environment);
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "{origin}: invalid for the {} environment: {}",
                    self.environment,
                    problems.join("; ")
                ),
            ));
        }
        Ok(typed)
    }

    /// Validates every file under `config_dir` against the schema its
    /// location implies, and returns one message per problem; empty means
    /// valid. A file in no known location is a problem, so nothing in
    /// `config/` goes unvalidated. `Err` only if the directory cannot be read.
    pub fn validate_dir(&self, config_dir: &Path) -> Result<Vec<String>, Error> {
        let mut files = Vec::new();
        collect_files(config_dir, &mut files)?;
        files.sort();
        let mut problems = Vec::new();
        for path in files {
            let relative = relative_path(config_dir, &path);
            let checked = match relative.as_str() {
                "context.yaml" => self.load::<ContextBudgets>(&path).map(|_| ()),
                "policies/autonomy.yaml" => self.load::<AutonomyPolicy>(&path).map(|_| ()),
                "policies/context.yaml" => self.load::<ContextPolicy>(&path).map(|_| ()),
                "policies/gates.yaml" => self.load::<GatesPolicy>(&path).map(|_| ()),
                "models/routing.yaml" => self.load::<ModelRouting>(&path).map(|_| ()),
                "runtime.yaml" => self.load::<RuntimeSettings>(&path).map(|_| ()),
                "worker.yaml" => self.load::<WorkerSettings>(&path).map(|_| ()),
                _ if in_dir(&relative, "tools") => self
                    .load::<ToolOverlay>(&path)
                    .and_then(|overlay| overlay_schemas_exist(&overlay, &path)),
                _ if in_dir(&relative, "agents") => self.load::<AgentSpec>(&path).map(|_| ()),
                _ => Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("{}: no schema applies to config/{relative}", path.display()),
                )),
            };
            if let Err(e) = checked {
                problems.push(e.message().to_owned());
            }
        }
        Ok(problems)
    }
}

/// A tool overlay names its tools' request and result schemas; each must be a
/// file in `schemas/`, or a typo would pass validation unnoticed.
fn overlay_schemas_exist(overlay: &ToolOverlay, path: &Path) -> Result<(), Error> {
    let missing: Vec<String> = overlay
        .tools
        .iter()
        .flat_map(|tool| {
            [
                ("request_schema", &tool.request_schema),
                ("result_schema", &tool.result_schema),
            ]
            .into_iter()
            .filter_map(move |(field, schema)| {
                let schema = schema.as_ref()?.as_str();
                let known = SCHEMA_FILES.iter().any(|(p, _)| *p == schema);
                (!known).then(|| format!("tool {}: {field} {schema}", tool.name.as_str()))
            })
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::InvalidInput,
        format!(
            "{}: not a schema in schemas/: {}",
            path.display(),
            missing.join("; ")
        ),
    ))
}

pub(crate) fn read_text(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|e| {
        Error::new(
            ErrorCode::NotFound,
            format!("{}: cannot read: {e}", path.display()),
        )
        .with_source(e)
    })
}

/// Parses one YAML document into a JSON value.
pub(crate) fn yaml_value(text: &str, origin: &str) -> Result<Value, Error> {
    // Options is non-exhaustive: start from the defaults, which keep the
    // parsing budget on.
    let mut options = Options::default();
    options.strict_booleans = true;
    options.duplicate_keys = DuplicateKeyPolicy::Error;
    options.merge_keys = MergeKeyPolicy::Error;
    options.reject_unsupported_tags = true;
    serde_saphyr::from_str_with_options(text, options).map_err(|e| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("{origin}: invalid YAML: {e}"),
        )
    })
}

/// `name.yaml` directly inside `dir`.
fn in_dir(relative: &str, dir: &str) -> bool {
    relative
        .strip_prefix(dir)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.contains('/') && name.ends_with(".yaml"))
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|e| {
        Error::new(
            ErrorCode::NotFound,
            format!("{}: cannot read the directory: {e}", dir.display()),
        )
        .with_source(e)
    })?;
    for entry in entries {
        let path = entry
            .map_err(|e| {
                Error::new(
                    ErrorCode::Internal,
                    format!("{}: cannot list the directory", dir.display()),
                )
                .with_source(e)
            })?
            .path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_matched_by_location() {
        assert!(in_dir("tools/build123d.yaml", "tools"));
        assert!(!in_dir("tools/nested/x.yaml", "tools"));
        assert!(!in_dir("tools/notes.md", "tools"));
        assert!(!in_dir("toolsx/a.yaml", "tools"));
    }
}
