//! Increment 1's exit criteria 1–5 through the `synth` CLI (ADR-0034): the
//! `synth`, `api-server` and `worker` binaries, the build123d server through
//! its forwarder, and a scripted model on a fake Pliny server.
//!
//! The dev benchmark's request starts a run. Worker A is killed while its
//! third model call is held, and `synth run watch` is killed mid-stream.
//! Worker B reclaims the job; a second `watch` resumes from the journal,
//! sees the approval the autonomy policy asks for, and `synth run approve`
//! answers it. The run completes with a stop reason the checks decided,
//! and `synth run report` links every requirement to stored evidence.
//!
//! Ignored by default: it needs the forwarder (`BUILD123D_URL`) and the
//! three binaries built. `cargo xtask test-tools` provides them.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use api_client::{Client, ClientConfig, ClientError, Token};
use bootstrap::{API_TOKEN_ENV, Overrides, WorkerSetup, build_worker};
use contracts::ids::RunId;
use contracts::types::{
    AgentSpec, Event, EventPayload, JobState, RequirementStatus, RunReport, StopReason, Verdict,
};
use infra::benchmarks::BenchmarkLoader;
use infra::config::ConfigLoader;
use serde_json::Value;
use testkit::builders::id;
use testkit::fakes::FakePlinyServer;

use crate::support::db::TestDb;
use crate::support::*;
use crate::worker_process::{binary, log_of, spawn};

/// Real CAD tools and process starts take longer than the fakes.
const PROCESS_WAIT: Duration = Duration::from_secs(120);
const TOKEN: &str = "e2e-test-token-0123456789abcdef";
const PROBLEM: &str = "part_design_001";
const REQUIREMENTS: [&str; 4] = ["R1", "R2", "R3", "R4"];

/// A child process, killed when dropped so a failed assertion leaves none
/// behind.
struct Process(Child);

impl Process {
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The CLI as a user runs it, against `api`.
struct Synth {
    binary: PathBuf,
    api: String,
    state_dir: PathBuf,
}

impl Synth {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env("SYNTH_API_URL", &self.api)
            .env("SYNTH_API_TOKEN", TOKEN)
            .env_remove("RUST_LOG");
        command
    }

    /// Runs a command to its end; panics unless it exits 0. Its stdout.
    fn run(&self, args: &[&str]) -> String {
        let Output {
            status,
            stdout,
            stderr,
        } = self
            .command(args)
            .output()
            .unwrap_or_else(|e| panic!("synth {args:?} does not start: {e}"));
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(status.success(), "synth {args:?}: {status}\n{stderr}");
        String::from_utf8(stdout).expect("UTF-8 output")
    }

    /// Starts `synth run watch`, printing to `<out>`.
    fn watch(&self, run: &str, out: &Path) -> Process {
        let out = File::create(out).expect("an output file");
        let child = self
            .command(&["run", "watch", run, "--state-dir"])
            .arg(&self.state_dir)
            .stdout(out)
            .stderr(Stdio::null())
            .spawn()
            .expect("start synth run watch");
        Process(child)
    }

    fn journal(&self, run: &str) -> PathBuf {
        self.state_dir.join("runs").join(run).join("events.jsonl")
    }
}

/// The complete lines of a journal, as events.
fn journaled(path: &Path) -> Vec<Event> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let complete = text.rfind('\n').map_or("", |end| &text[..=end]);
    complete
        .lines()
        .map(|line| serde_json::from_str(line).expect("a journaled event"))
        .collect()
}

/// Polls until `found` returns something, or panics after the wait.
async fn eventually<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + PROCESS_WAIT;
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A free loopback address for api-server.
fn free_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("its address")
}

/// The run request of the dev benchmark, as `cargo xtask benchmark-request`
/// builds it.
fn benchmark_request(config: &Path) -> Value {
    let problem = BenchmarkLoader::new()
        .expect("the benchmark schema")
        .load(&repo_root().join(format!("evals/benchmarks/dev/{PROBLEM}/problem.yaml")))
        .expect("the dev problem");
    let agent: AgentSpec = ConfigLoader::new(contracts::Environment::Test)
        .expect("the config schemas")
        .load(&config.join("agents/main_orchestrator.yaml"))
        .expect("the agent spec");
    let request = evaluation::benchmarks::run_request(&problem, &agent).expect("a request");
    serde_json::to_value(request).expect("serializable")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the build123d forwarder and the synth, api-server and worker binaries; run by cargo xtask test-tools"]
async fn the_dev_benchmark_runs_end_to_end_through_the_cli() {
    let tools = std::env::var("BUILD123D_URL")
        .expect("BUILD123D_URL must point at the build123d forwarder (cargo xtask test-tools)");
    let synth_binary = binary("synth", "synth-cli");
    let api_binary = binary("api-server", "synth-api-server");
    let worker_binary = binary("worker", "synth-worker");
    let model = FakePlinyServer::start(plate_script::benchmark_plate, Some(2)).await;
    let db = TestDb::create().await;
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let artifacts = scratch.path().join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("an artifact volume");

    let address = free_address();
    let worker_yaml = TEST_WORKER_YAML
        .replace("http://127.0.0.1:9/mcp", &tools)
        .replace("connect_timeout_ms: 1000", "connect_timeout_ms: 10000");
    let api_yaml = std::fs::read_to_string(repo_root().join("config/api.yaml"))
        .expect("the committed api settings")
        .replace("listen: 127.0.0.1:8080", &format!("listen: {address}"))
        .replace(
            "artifact_root: runtime-data/artifacts",
            &format!("artifact_root: {}", artifacts.display()),
        );
    // The committed autonomy policy asks before the final commit.
    let config = TestConfig::new(&[
        ("worker.yaml", worker_yaml.as_str()),
        ("api.yaml", api_yaml.as_str()),
    ]);
    let logs = scratch.path().join("logs");
    std::fs::create_dir_all(&logs).expect("a log directory");
    let mut vars = vars(&db);
    vars.insert("PLINY_API_KEY".to_owned(), "test-key".to_owned());
    vars.insert("PLINY_BASE_URL".to_owned(), model.base_url());
    vars.insert(API_TOKEN_ENV.to_owned(), TOKEN.to_owned());

    // Reads the stores the processes write; it never runs.
    let watcher = build_worker(
        WorkerSetup {
            config_dir: config.path().to_owned(),
            prompts_dir: repo_root().join("prompts"),
            worker_id: Some("watcher".to_owned()),
            overrides: Overrides::default(),
        },
        &vars,
    )
    .await
    .unwrap_or_else(|e| panic!("the watcher does not build: {e}"));

    let _api = Process(api_server(&api_binary, config.path(), &logs, &vars));
    let api = format!("http://{address}");
    wait_until_serving(&api, &logs).await;
    let synth = Synth {
        binary: synth_binary,
        api,
        state_dir: scratch.path().join("cli"),
    };

    // 1. The dev benchmark's request, started through the CLI.
    let request_path = scratch.path().join("request.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec_pretty(&benchmark_request(config.path())).expect("json"),
    )
    .expect("the request file");
    let request_arg = request_path.to_str().expect("a UTF-8 path");
    let started: Value = serde_json::from_str(&synth.run(&[
        "run",
        "start",
        request_arg,
        "--idempotency-key",
        "e2e-benchmark-1",
    ]))
    .expect("the run as JSON");
    let run = started["run_id"].as_str().expect("a run id").to_owned();
    let journal = synth.journal(&run);

    // 2 and 3. Worker A is killed on its held third model call, and the
    // first watch is killed mid-stream.
    let mut a = Process(spawn(
        &worker_binary,
        config.path(),
        &logs,
        "worker-a",
        &vars,
    ));
    let mut first_watch = synth.watch(&run, &scratch.path().join("watch-1.out"));
    eventually("worker A's third model call", || {
        (model.requests().len() >= 3).then_some(())
    })
    .await;
    model.held(PROCESS_WAIT).await;
    eventually("the first watch to journal some events", || {
        (journaled(&journal).len() >= 3).then_some(())
    })
    .await;
    a.kill();
    first_watch.kill();
    let before = journaled(&journal).len();

    let _b = Process(spawn(
        &worker_binary,
        config.path(),
        &logs,
        "worker-b",
        &vars,
    ));
    let mut second_watch = synth.watch(&run, &scratch.path().join("watch-2.out"));

    // 4. The approval, seen in the stream and answered through the CLI.
    let approval = eventually("the approval in the journal", || {
        journaled(&journal).iter().find_map(|e| match &e.payload {
            EventPayload::ApprovalRequested { approval_id, .. } => {
                Some(approval_id.as_str().to_owned())
            }
            _ => None,
        })
    })
    .await;
    let answered: Event = serde_json::from_str(
        &synth.run(&["run", "approve", &run, &approval, "--answer", "approve"]),
    )
    .expect("the stored answer");
    assert!(matches!(
        answered.payload,
        EventPayload::ApprovalAnswered { .. }
    ));

    let watched = eventually("the second watch to end with the run", || {
        second_watch.0.try_wait().expect("its status")
    })
    .await;
    assert!(
        watched.success(),
        "watch exited {watched}; worker B:\n{}",
        log_of(&logs, "worker-b")
    );

    // The run as stored.
    let run_id: RunId = id(&run);
    let job = watcher
        .jobs
        .find_by_run(&run_id)
        .await
        .expect("the job store")
        .expect("the run's job");
    assert_eq!(
        contracts::job_state(&job.status),
        JobState::Completed,
        "worker B:\n{}",
        log_of(&logs, "worker-b")
    );
    invariants::hold(&watcher, &job, None).await;
    let stored = logged(&watcher, &run).await;

    // 3. The journal holds every event once, as stored, across the kill;
    // the second watch printed only what came after the first.
    assert_eq!(journaled(&journal), stored, "no gaps and no repeats");
    let printed: Vec<Event> = std::fs::read_to_string(scratch.path().join("watch-2.out"))
        .expect("the second watch's output")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a printed event"))
        .collect();
    assert_eq!(printed.as_slice(), &stored[before..]);

    // 2. Nothing was run twice after the kill.
    let started_steps: Vec<String> = stored
        .iter()
        .map(|e| serde_json::to_value(&e.payload).expect("a payload"))
        .filter(|v| v["type"] == "tool_call_started")
        .map(|v| v["step_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    let distinct: BTreeSet<&String> = started_steps.iter().collect();
    assert_eq!(
        distinct.len(),
        started_steps.len(),
        "a step was started twice: {started_steps:?}"
    );

    // 1 and 5. The report through the CLI: success, decided by checks, and
    // every requirement linked to evidence that exists.
    let report: RunReport =
        serde_json::from_str(&synth.run(&["run", "report", &run])).expect("the report as JSON");
    assert!(report.final_);
    assert_eq!(report.stop_reason, StopReason::Success);
    let rows: HashMap<&str, _> = report
        .requirements
        .iter()
        .map(|row| (row.requirement_id.as_str(), row))
        .collect();
    for requirement in REQUIREMENTS {
        let row = rows
            .get(requirement)
            .unwrap_or_else(|| panic!("{requirement} is not in the report"));
        assert_eq!(row.status, RequirementStatus::Pass, "{requirement}");
        assert!(
            row.evidence
                .iter()
                .any(|f| f.used && f.verdict == Verdict::Pass),
            "{requirement} passes on no finding"
        );
        for finding in &row.evidence {
            watcher
                .evidence
                .get(&finding.evidence_id)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{requirement} links {} which is not stored: {e}",
                        finding.evidence_id.as_str()
                    )
                });
        }
    }

    drop(watcher);
    db.drop().await;
}

/// Starts the api-server binary on `config`, logging to `<logs>/api-server.log`.
fn api_server(binary: &Path, config: &Path, logs: &Path, vars: &HashMap<String, String>) -> Child {
    let log = File::create(logs.join("api-server.log")).expect("a log file");
    Command::new(binary)
        .arg("--config")
        .arg(config)
        .arg("--prompts")
        .arg(repo_root().join("prompts"))
        .envs(vars)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("start the api-server binary")
}

/// Waits until api-server answers: a request for an unknown run is 404.
async fn wait_until_serving(api: &str, logs: &Path) {
    let client = Client::new(
        api,
        Token::new(TOKEN.to_owned()).expect("a token"),
        ClientConfig {
            retries: 0,
            request_timeout: Duration::from_secs(2),
            ..ClientConfig::default()
        },
    )
    .expect("a client");
    let deadline = tokio::time::Instant::now() + PROCESS_WAIT;
    loop {
        match client.run(&id("probe")).await {
            Err(ClientError::Api { status: 404, .. }) => return,
            Err(ClientError::Transport(_) | ClientError::Timeout(_)) => {}
            other => panic!(
                "api-server answered {other:?}:\n{}",
                log_of(logs, "api-server")
            ),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "api-server did not start:\n{}",
            log_of(logs, "api-server")
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
