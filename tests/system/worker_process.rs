//! A real worker process is killed mid-run and another finishes the job
//! (ADR-0031): the `worker` binary, the build123d server through its
//! forwarder, and a fake Claude server on loopback that holds the third
//! model call. Worker A is killed while that call is in flight; worker B,
//! a new process, reclaims the job once A's lease expires.
//!
//! Ignored by default: it needs the forwarder (`BUILD123D_URL`) and the
//! `worker` binary built. `cargo xtask test-tools` provides both.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bootstrap::{Overrides, WorkerSetup, build_worker};
use contracts::types::JobState;
use serde_json::{Value, json};
use testkit::fakes::{FakeClaudeServer, message};

use crate::support::db::TestDb;
use crate::support::*;

/// Real CAD tools take longer than the fakes.
const PROCESS_WAIT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the build123d forwarder and the worker binary; run by cargo xtask test-tools"]
async fn a_killed_worker_process_is_recovered_by_the_next() {
    let tools = std::env::var("BUILD123D_URL")
        .expect("BUILD123D_URL must point at the build123d forwarder (cargo xtask test-tools)");
    let binary = worker_binary();
    let server = FakeClaudeServer::start(answer, Some(2)).await;
    let db = TestDb::create().await;
    let worker_yaml = TEST_WORKER_YAML
        .replace("http://127.0.0.1:9/mcp", &tools)
        .replace("connect_timeout_ms: 1000", "connect_timeout_ms: 10000");
    let routing = std::fs::read_to_string(repo_root().join("config/models/routing.yaml"))
        .expect("the committed routing")
        .replace(
            "base_url: https://api.anthropic.com",
            &format!("base_url: {}", server.base_url()),
        );
    let config = TestConfig::new(&[
        ("worker.yaml", worker_yaml.as_str()),
        ("models/routing.yaml", routing.as_str()),
        ("policies/autonomy.yaml", NO_APPROVALS_YAML),
    ]);
    let logs = tempfile::tempdir().expect("a log directory");
    let mut vars = vars(&db);
    vars.insert("ANTHROPIC_API_KEY".to_owned(), "test-key".to_owned());

    // Built like the processes, to seed and submit; it never runs.
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
    seed(&watcher).await;
    let submitted = job("job-1", "run-1");
    watcher
        .scheduler
        .submit(&submitted, None)
        .await
        .expect("submit");

    let mut a = spawn(&binary, config.path(), logs.path(), "worker-a", &vars);
    let deadline = tokio::time::Instant::now() + PROCESS_WAIT;
    while server.requests().len() < 3 {
        let exited = a.try_wait().expect("worker A's status");
        assert!(
            exited.is_none() && tokio::time::Instant::now() < deadline,
            "worker A {} before its third model call:\n{}",
            exited.map_or("timed out".to_owned(), |s| format!("exited ({s})")),
            log_of(logs.path(), "worker-a")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server.held(PROCESS_WAIT).await;
    a.kill().expect("kill worker A");
    a.wait().expect("worker A exits");

    let mut b = spawn(&binary, config.path(), logs.path(), "worker-b", &vars);
    let stored = match tokio::time::timeout(
        PROCESS_WAIT,
        wait_until(&watcher, "job-1", |state| state == JobState::Completed),
    )
    .await
    {
        Ok(stored) => stored,
        Err(_) => panic!(
            "worker B did not finish:\n{}",
            log_of(logs.path(), "worker-b")
        ),
    };
    b.kill().expect("stop worker B");
    b.wait().expect("worker B exits");

    invariants::hold(&watcher, &stored, None).await;
    let started: Vec<String> = logged(&watcher, "run-1")
        .await
        .iter()
        .map(|e| serde_json::to_value(&e.payload).expect("a payload"))
        .filter(|v| v["type"] == "tool_call_started")
        .map(|v| v["step_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    let distinct: BTreeSet<&String> = started.iter().collect();
    assert_eq!(
        distinct.len(),
        started.len(),
        "a step was started twice: {started:?}"
    );
    assert_eq!(
        server.requests().len(),
        4,
        "three calls by A, then the third again by B"
    );
    drop(watcher);
    db.drop().await;
}

/// The `worker` binary next to this test's directory in the target
/// directory, as `cargo build -p synth-worker` leaves it.
fn worker_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("the test's own path");
    let binary = exe
        .parent()
        .and_then(Path::parent)
        .expect("the target directory")
        .join(format!("worker{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.exists(),
        "{} is missing: run cargo build -p synth-worker first (cargo xtask test-tools does)",
        binary.display()
    );
    binary
}

/// What the worker `id` logged.
fn log_of(logs: &Path, id: &str) -> String {
    std::fs::read_to_string(logs.join(format!("{id}.log"))).unwrap_or_default()
}

/// Starts the worker binary as `id` on `config`, logging to `<logs>/<id>.log`.
fn spawn(
    binary: &Path,
    config: &Path,
    logs: &Path,
    id: &str,
    vars: &HashMap<String, String>,
) -> Child {
    let log = File::create(logs.join(format!("{id}.log"))).expect("a log file");
    Command::new(binary)
        .args(["--config"])
        .arg(config)
        .arg("--prompts")
        .arg(repo_root().join("prompts"))
        .args(["--id", id])
        .envs(vars)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("start the worker binary")
}

/// The policy model's answer, over the Messages API: create the plate,
/// measure the part it returned, then propose the measured mass.
fn answer(request: &Value) -> Value {
    let mut tools: HashMap<String, String> = HashMap::new();
    let mut results: HashMap<String, Value> = HashMap::new();
    let mut calls = 0;
    let blocks = request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().into_iter().flatten());
    for block in blocks {
        match block["type"].as_str() {
            Some("tool_use") => {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                tools.insert(id, block["name"].as_str().unwrap_or_default().to_owned());
                calls += 1;
            }
            Some("tool_result") if block["is_error"] != true => {
                let tool = tools.get(block["tool_use_id"].as_str().unwrap_or_default());
                let content = envelope_json(block["content"].as_str().unwrap_or_default());
                if let (Some(tool), Some(content)) = (tool, content)
                    && content["status"] == "ok"
                {
                    results.insert(tool.clone(), content);
                }
            }
            _ => {}
        }
    }
    let use_tool = |name: &str, input: Value| {
        message(vec![json!({
            "type": "tool_use",
            "id": format!("toolu_{}", calls + 1),
            "name": name,
            "input": input
        })])
    };
    match (
        results.get("create_plate"),
        results.get("measure"),
        results.contains_key("sdr_propose"),
    ) {
        (None, _, _) => use_tool("create_plate", plate()),
        (Some(plate), None, _) => use_tool(
            "measure",
            json!({ "part": plate["result"]["part"], "density": { "value": 2.70, "unit": "g/cm3" } }),
        ),
        (Some(_), Some(measured), false) => {
            let mass = measured["result"]["mass"]["value"]
                .as_f64()
                .unwrap_or_default();
            let evidence: Vec<String> = measured["evidence_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect();
            use_tool("sdr_propose", propose_mass(mass, &evidence, true))
        }
        (Some(_), Some(_), true) => message(vec![json!({ "type": "text", "text": "Done." })]),
    }
}
