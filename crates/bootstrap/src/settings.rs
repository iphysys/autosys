//! What a worker is built from: the environment it runs in, its id, and the
//! sections of `config/`, each validated for that environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use contracts::Environment;
use contracts::ids::Identifier;
use contracts::types::{
    AgentSpec, AutonomyPolicy, ContextBudgets, ContextPolicy, GatesPolicy, ModelRouting,
    RuntimeSettings, ToolOverlay, WorkerSettings,
};
use infra::config::{ConfigFile, ConfigLoader};
use model_gateway::Env;

use crate::BootstrapError;

/// The variable that names the worker, unless the command line does.
pub const WORKER_ID_ENV: &str = "SYNTH_WORKER_ID";
/// The variable that holds the database URL. It may carry credentials, so
/// it is never logged or written into config.
pub const DATABASE_URL_ENV: &str = "DATABASE_URL";

/// The environment `SYNTH_ENVIRONMENT` names; unset is Production.
pub fn environment(env: &dyn Env) -> Result<Environment, BootstrapError> {
    Environment::from_var(env.var(Environment::VAR).as_deref())
        .map_err(|e| BootstrapError::check("environment", e))
}

/// The worker's id: `given` (the command line), else `SYNTH_WORKER_ID`,
/// else `worker-<pid>-<start ms>`, which no other process shares.
pub fn worker_id(given: Option<&str>, env: &dyn Env) -> Result<Identifier, BootstrapError> {
    let id = match given {
        Some(given) => given.to_owned(),
        None => match env.var(WORKER_ID_ENV) {
            Some(id) => id,
            None => {
                let started = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                format!("worker-{}-{started}", std::process::id())
            }
        },
    };
    id.parse().map_err(|_| {
        BootstrapError::new(
            "worker id",
            format!(
                "`{id}` is not an identifier: a letter or digit, then up to 127 letters, \
                 digits, `_`, `.`, `:` or `-`"
            ),
        )
    })
}

/// Every section of `config/` the worker uses.
pub(crate) struct Config {
    pub(crate) runtime: RuntimeSettings,
    pub(crate) worker: WorkerSettings,
    pub(crate) context_budgets: ContextBudgets,
    pub(crate) context_policy: ContextPolicy,
    pub(crate) gates: GatesPolicy,
    pub(crate) autonomy: AutonomyPolicy,
    pub(crate) routing: ModelRouting,
    /// `config/tools/*.yaml`, in file name order.
    pub(crate) overlays: Vec<ToolOverlay>,
    /// `config/agents/*.yaml`, in file name order.
    pub(crate) agents: Vec<AgentSpec>,
}

impl Config {
    /// Validates every file under `dir` for `environment`, reporting every
    /// problem at once, then reads each section.
    pub(crate) fn load(dir: &Path, environment: Environment) -> Result<Self, BootstrapError> {
        let config = |e| BootstrapError::check("config", e);
        let loader = ConfigLoader::new(environment).map_err(config)?;
        let problems = loader.validate_dir(dir).map_err(config)?;
        if !problems.is_empty() {
            return Err(BootstrapError::new(
                "config",
                format!(
                    "{} is invalid for the {environment} environment: {}",
                    dir.display(),
                    problems.join("; ")
                ),
            ));
        }
        let load = |path: &str| dir.join(path);
        Ok(Self {
            runtime: loader.load(&load("runtime.yaml")).map_err(config)?,
            worker: loader.load(&load("worker.yaml")).map_err(config)?,
            context_budgets: loader.load(&load("context.yaml")).map_err(config)?,
            context_policy: loader
                .load(&load("policies/context.yaml"))
                .map_err(config)?,
            gates: loader.load(&load("policies/gates.yaml")).map_err(config)?,
            autonomy: loader
                .load(&load("policies/autonomy.yaml"))
                .map_err(config)?,
            routing: loader.load(&load("models/routing.yaml")).map_err(config)?,
            overlays: load_all(&loader, &load("tools"))?,
            agents: load_all(&loader, &load("agents"))?,
        })
    }
}

/// Every YAML file directly in `dir`, in file name order.
fn load_all<T: ConfigFile>(loader: &ConfigLoader, dir: &Path) -> Result<Vec<T>, BootstrapError> {
    let unreadable = |e: std::io::Error| {
        BootstrapError::new("config", format!("cannot read {}: {e}", dir.display()))
    };
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(unreadable)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(unreadable)?;
    paths.retain(|p| p.extension().is_some_and(|x| x == "yaml"));
    paths.sort();
    paths
        .iter()
        .map(|path| {
            loader
                .load(path)
                .map_err(|e| BootstrapError::check("config", e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_command_line_names_the_worker_before_the_environment() {
        let env = vars(&[(WORKER_ID_ENV, "from-env")]);
        let id = worker_id(Some("from-args"), &env).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(id.as_str(), "from-args");
        let id = worker_id(None, &env).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(id.as_str(), "from-env");
    }

    #[test]
    fn an_unnamed_worker_gets_its_process_id() {
        let id = worker_id(None, &vars(&[])).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            id.as_str()
                .starts_with(&format!("worker-{}-", std::process::id())),
            "{}",
            id.as_str()
        );
    }

    #[test]
    fn an_invalid_worker_id_fails_startup() {
        let error = worker_id(Some("no spaces"), &vars(&[])).err();
        assert!(
            matches!(
                error,
                Some(BootstrapError::Startup {
                    check: "worker id",
                    ..
                })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn the_environment_defaults_to_production_and_refuses_an_unknown_name() {
        assert_eq!(environment(&vars(&[])).ok(), Some(Environment::Production));
        assert_eq!(
            environment(&vars(&[(Environment::VAR, "test")])).ok(),
            Some(Environment::Test)
        );
        assert!(environment(&vars(&[(Environment::VAR, "staging")])).is_err());
    }

    #[test]
    fn the_committed_config_loads_for_production() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let config = Config::load(&dir, Environment::Production).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.overlays.len(), 1);
        assert_eq!(config.agents.len(), 1);
    }
}
