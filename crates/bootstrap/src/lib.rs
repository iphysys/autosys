//! The composition root (blueprint section 13.1): the only crate that sees
//! every other, and the only place that builds stores, clients and
//! providers. [`build_worker`] runs the startup checks and builds a worker
//! from `config/`, `prompts/` and the environment, in a fixed order. Any
//! failure there is a [`BootstrapError`] and the worker does not start.
//!
//! Only here is the SDR repository adapter built, and only `SdrManager`
//! receives it (section 7.2). Bootstrap grants no write authority: the
//! scheduler builds every context a run writes with (ADR-0032).
//!
//! Tests replace parts through [`Overrides`]: model providers, tool servers,
//! a wrapper around each store, and the spawner. Product code passes none.

mod checks;
mod model;
mod settings;
mod spawn;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use context::{ContextBuilder, ContextProfiles, PromptLibrary};
use contracts::ids::Identifier;
use contracts::{
    Check, Clock, Environment, EventLog, EvidenceStore, ExecutionContext, Gate, GateSubjectKind,
    JobStore, ModelProvider, SdrRepository, Spawner, StepStore, SystemClock,
};
use evaluation::Evaluator;
use evaluation::checks::{NumericThresholdCheck, SchemaCheck, UnitsCheck};
use governance::{ApprovalGate, GateRunner, SdrCommitGate, ToolPermissionGate};
use infra::persistence::{
    Database, PgEventLog, PgEvidenceStore, PgJobStore, PgSdrRepository, PgStepStore, PoolSettings,
};
use model_gateway::{ModelGateway, ProviderAdapter, Routing};
use runtime::{AgentRegistry, Components, Orchestrator, Scheduler, SchedulerSettings};
use sdr::SdrManager;
use tokio_util::task::TaskTracker;
use tools::{McpServer, McpSettings, ServerTools, ToolGateway, ToolRegistry};

pub use model_gateway::{Env, ProcessEnv};
pub use settings::{DATABASE_URL_ENV, WORKER_ID_ENV, environment, worker_id};
pub use spawn::TokioSpawner;
pub use tokio_util::sync::CancellationToken;

/// The build recorded with every commit.
const HARNESS_BUILD: &str = concat!("autosys ", env!("CARGO_PKG_VERSION"));

/// A startup failure: the worker must not start (exit code 2).
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("startup check `{check}` failed: {message}")]
    Startup {
        /// What was being checked or built.
        check: &'static str,
        message: String,
        #[source]
        source: Option<contracts::Error>,
    },
}

impl BootstrapError {
    pub(crate) fn new(check: &'static str, message: impl Into<String>) -> Self {
        Self::Startup {
            check,
            message: message.into(),
            source: None,
        }
    }

    /// `error` stopped `check`.
    pub(crate) fn check(check: &'static str, error: contracts::Error) -> Self {
        Self::Startup {
            check,
            message: error.message().to_owned(),
            source: Some(error),
        }
    }

    /// A component's own error type stopped `check`.
    fn other(check: &'static str, error: impl std::fmt::Display) -> Self {
        Self::new(check, error.to_string())
    }
}

/// Wraps a store as it is built: a test's pausing double around the real
/// one. The same shape as testkit's `faults` wrappers.
pub type Wrap<T> = Box<dyn FnOnce(Arc<T>) -> Arc<T> + Send>;

/// Parts a test replaces. The default replaces nothing.
#[derive(Default)]
pub struct Overrides {
    /// Model providers by profile name, used instead of the ones
    /// `config/models/routing.yaml` describes. Every profile in use needs
    /// one, and none needs its environment variables.
    pub models: Option<BTreeMap<String, Arc<dyn ModelProvider>>>,
    /// Tool servers used instead of connecting to the ones `worker.yaml`
    /// lists. The overlays in `config/tools/` still apply.
    pub tool_servers: Option<Vec<ServerTools>>,
    pub jobs: Option<Wrap<dyn JobStore>>,
    pub events: Option<Wrap<dyn EventLog>>,
    pub steps: Option<Wrap<dyn StepStore>>,
    /// Around the SDR repository, which only `SdrManager` receives.
    pub sdr_storage: Option<Wrap<dyn SdrRepository>>,
    /// Starts the worker's background tasks instead of [`TokioSpawner`].
    pub spawner: Option<Arc<dyn Spawner>>,
}

/// What a worker is built from.
pub struct WorkerSetup {
    /// The `config/` directory.
    pub config_dir: PathBuf,
    /// The `prompts/` directory.
    pub prompts_dir: PathBuf,
    /// The worker id the command line gave, if any (see [`worker_id`]).
    pub worker_id: Option<String>,
    pub overrides: Overrides,
}

/// A built worker. The stores are exposed for tests and for the binary's
/// logs; only the scheduler writes through them.
pub struct Worker {
    pub id: Identifier,
    pub environment: Environment,
    pub scheduler: Arc<Scheduler>,
    pub jobs: Arc<dyn JobStore>,
    pub events: Arc<dyn EventLog>,
    pub steps: Arc<dyn StepStore>,
    pub evidence: Arc<dyn EvidenceStore>,
    pub sdr: Arc<SdrManager>,
    tracker: TaskTracker,
}

impl Worker {
    /// Claims and runs jobs until `shutdown`. A job running then goes back
    /// to the queue, which is not a failed attempt.
    pub async fn run(&self, shutdown: CancellationToken) {
        tracing::info!(worker = self.id.as_str(), environment = %self.environment, "worker started");
        self.scheduler.run(shutdown).await;
        tracing::info!(worker = self.id.as_str(), "worker stopped claiming");
    }

    /// Waits for every background task the worker started to end.
    pub async fn drain(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

/// Runs the startup checks and builds a worker, reading environment
/// variables from `env` only. In order: the environment and worker id;
/// `config/` for that environment; `prompts/`; the database, migrated; the
/// stores and the SDR manager; the tool servers, registry and gateway; the
/// checks and gates; the context builder and agents; the model gateway;
/// the orchestrator and the scheduler.
pub async fn build_worker(
    setup: WorkerSetup,
    env: &(dyn Env + Sync),
) -> Result<Worker, BootstrapError> {
    let WorkerSetup {
        config_dir,
        prompts_dir,
        worker_id: given_id,
        overrides,
    } = setup;
    install_crypto_provider();
    let environment = settings::environment(env)?;
    let id = settings::worker_id(given_id.as_deref(), env)?;
    let config = settings::Config::load(&config_dir, environment)?;
    let prompts =
        PromptLibrary::load_dir(&prompts_dir).map_err(|e| BootstrapError::other("prompts", e))?;
    let url = checks::database_url(env)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // The database and the stores.
    let database = &config.worker.database;
    let db = Database::connect(
        &url,
        &PoolSettings {
            max_connections: u32::try_from(database.max_connections.get()).unwrap_or(u32::MAX),
            acquire_timeout: Duration::from_millis(database.acquire_timeout_ms.get()),
        },
        Duration::from_millis(database.operation_timeout_ms.get()),
    )
    .await
    .map_err(|e| BootstrapError::check("database", e))?;
    db.migrate()
        .await
        .map_err(|e| BootstrapError::check("database", e))?;
    let mut overrides = overrides;
    let jobs = wrap(
        Arc::new(PgJobStore::new(db.clone())) as Arc<dyn JobStore>,
        overrides.jobs.take(),
    );
    let events = wrap(
        Arc::new(
            PgEventLog::connect(db.clone())
                .await
                .map_err(|e| BootstrapError::check("database", e))?,
        ) as Arc<dyn EventLog>,
        overrides.events.take(),
    );
    let steps = wrap(
        Arc::new(PgStepStore::new(db.clone())) as Arc<dyn StepStore>,
        overrides.steps.take(),
    );
    let evidence: Arc<dyn EvidenceStore> = Arc::new(PgEvidenceStore::new(db.clone()));
    let storage = wrap(
        Arc::new(PgSdrRepository::new(db)) as Arc<dyn SdrRepository>,
        overrides.sdr_storage.take(),
    );
    let sdr = Arc::new(
        SdrManager::new(storage, clock.clone()).map_err(|e| BootstrapError::other("sdr", e))?,
    );

    // Tools.
    let servers = match overrides.tool_servers.take() {
        Some(servers) => servers,
        None => connect_tool_servers(&config.worker.tool_servers).await?,
    };
    let registry = ToolRegistry::new(&config.overlays, servers)
        .map_err(|e| BootstrapError::check("tool servers", e))?;
    let available: Vec<String> = registry
        .available()
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let mut tool_gates: Vec<Arc<dyn Gate>> = vec![Arc::new(
        ToolPermissionGate::new(&config.overlays).map_err(|e| BootstrapError::check("gates", e))?,
    )];
    tool_gates.push(Arc::new(
        ApprovalGate::new(&config.autonomy, GateSubjectKind::ToolCall)
            .map_err(|e| BootstrapError::check("gates", e))?,
    ));
    let tool_gates = GateRunner::new(tool_gates, events.clone(), clock.clone())
        .map_err(|e| BootstrapError::check("gates", e))?;
    let tools = Arc::new(ToolGateway::new(
        registry,
        tool_gates,
        steps.clone(),
        evidence.clone(),
        events.clone(),
        clock.clone(),
    ));

    // Checks and the commit gates.
    let registered: Vec<Arc<dyn Check>> = vec![
        Arc::new(SchemaCheck::new().map_err(|e| BootstrapError::check("checks", e))?),
        Arc::new(UnitsCheck::new().map_err(|e| BootstrapError::check("checks", e))?),
        Arc::new(NumericThresholdCheck::new().map_err(|e| BootstrapError::check("checks", e))?),
    ];
    checks::registered_checks_are_required(&registered, &config.gates)?;
    let evaluator = Arc::new(
        Evaluator::new(registered, evidence.clone(), events.clone(), clock.clone())
            .map_err(|e| BootstrapError::check("checks", e))?,
    );
    let commit_gates: Vec<Arc<dyn Gate>> = vec![
        Arc::new(SdrCommitGate::new(&config.gates).map_err(|e| BootstrapError::check("gates", e))?),
        Arc::new(
            ApprovalGate::new(&config.autonomy, GateSubjectKind::SdrCommit)
                .map_err(|e| BootstrapError::check("gates", e))?,
        ),
    ];

    // Context and agents.
    let names: Vec<&str> = available.iter().map(String::as_str).collect();
    let agents = Arc::new(
        AgentRegistry::new(config.agents.clone(), &prompts, &names)
            .map_err(|e| BootstrapError::other("agents", e))?,
    );
    let profiles = || {
        ContextProfiles::new(config.context_budgets.clone(), &config.context_policy)
            .map_err(|e| BootstrapError::other("context", e))
    };
    let context = Arc::new(
        ContextBuilder::new(
            sdr.clone(),
            events.clone(),
            Arc::new(prompts),
            profiles()?,
            clock.clone(),
        )
        .map_err(|e| BootstrapError::other("context", e))?,
    );

    // Models.
    let routing = Routing::new(
        config.routing.clone(),
        Arc::new(
            model_gateway::providers::default_registry()
                .map_err(|e| BootstrapError::check("models", e))?,
        ),
        environment,
    )
    .map_err(|e| BootstrapError::check("models", e))?;
    let models = match overrides.models.take() {
        Some(given) => {
            let providers: BTreeMap<String, Arc<dyn ProviderAdapter>> = given
                .into_iter()
                .map(|(profile, provider)| {
                    (
                        profile,
                        Arc::new(model::Given(provider)) as Arc<dyn ProviderAdapter>,
                    )
                })
                .collect();
            ModelGateway::with_providers(routing, &agents.specs(), env, providers)
        }
        None => ModelGateway::start(routing, &agents.specs(), env),
    }
    .map_err(|e| BootstrapError::check("models", e))?;

    // The orchestrator and the scheduler.
    let harness_build = HARNESS_BUILD
        .parse()
        .map_err(|_| BootstrapError::new("orchestrator", "the harness build is empty"))?;
    let orchestrator = Arc::new(
        Orchestrator::new(Components {
            agents,
            sdr: sdr.clone(),
            context,
            context_profiles: profiles()?,
            tools,
            evaluator,
            evidence: evidence.clone(),
            commit_gates,
            models: Arc::new(models),
            events: events.clone(),
            clock: clock.clone(),
            settings: config.runtime.clone(),
            harness_build,
        })
        .map_err(|e| BootstrapError::other("orchestrator", e))?,
    );
    let tracker = TaskTracker::new();
    let spawner = overrides
        .spawner
        .take()
        .unwrap_or_else(|| Arc::new(TokioSpawner::new(tracker.clone())));
    let scheduler = Arc::new(Scheduler::new(
        jobs.clone(),
        events.clone(),
        sdr.clone(),
        orchestrator,
        spawner,
        clock,
        SchedulerSettings::new(id.clone(), &config.worker),
    ));
    Ok(Worker {
        id,
        environment,
        scheduler,
        jobs,
        events,
        steps,
        evidence,
        sdr,
        tracker,
    })
}

/// Installs ring as the process's TLS crypto provider, unless one is
/// already installed. reqwest is built without a provider of its own (the
/// model gateway picks ring in code), so a client that does not pick one,
/// such as the MCP transport's, uses this default.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Err means another thread installed one first, which is as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn wrap<T: ?Sized>(store: Arc<T>, wrapper: Option<Wrap<T>>) -> Arc<T> {
    match wrapper {
        Some(wrapper) => wrapper(store),
        None => store,
    }
}

/// Connects to each server `worker.yaml` lists. An unreachable server, or
/// one that does not offer what its overlay lists, stops startup.
async fn connect_tool_servers(
    servers: &[contracts::types::ToolServerConnection],
) -> Result<Vec<ServerTools>, BootstrapError> {
    let mut connected = Vec::new();
    for server in servers {
        let settings = McpSettings {
            connect_timeout: Duration::from_millis(server.connect_timeout_ms.get()),
            call_timeout: Duration::from_millis(server.call_timeout_ms.get()),
        };
        let ctx = ExecutionContext::builder(
            "corr-startup"
                .parse()
                .map_err(|_| BootstrapError::new("tool servers", "invalid correlation id"))?,
        )
        .build();
        let mcp = McpServer::connect(server.server_id.clone(), &server.url, &settings, &ctx)
            .await
            .map_err(|e| BootstrapError::check("tool servers", e))?;
        connected.push(mcp.into_server_tools());
    }
    Ok(connected)
}
