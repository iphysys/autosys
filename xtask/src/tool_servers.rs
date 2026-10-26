//! `cargo xtask test-tools`: each tool server's suite in its container, and
//! the sandbox it runs in (blueprint 17).
//!
//! Uses `deploy/compose/tools.yaml`. For each server it builds both images
//! and starts the server, then:
//!
//! 1. checks from outside, with `docker inspect`, that the container is on
//!    one network, that the network is internal, that no port is published,
//!    and that it runs as non-root, read-only, without capabilities and with
//!    CPU, memory and process limits;
//! 2. pipes `tests/egress_probe.py` into the running server container, which
//!    must reach nothing on the internet but must reach its own port, so a
//!    probe that cannot connect at all does not pass;
//! 3. starts the server's forwarder and checks it the same way: on the
//!    server's internal network and on one other, publishing one port on
//!    127.0.0.1 only, with the same hardening. The egress probe runs with
//!    the forwarder up, so a forwarder that gave the server a way out fails;
//! 4. runs the server's test container on the same network: tests (the same
//!    probes from a sibling, with the server as the control), lint and
//!    strict type check;
//! 5. runs the tool gateway's tests against the server, from the host
//!    through the forwarder (`crates/tools/tests/`);
//! 6. builds the `worker`, `api-server` and `synth` binaries and runs the
//!    system tests that run them as processes against the server through
//!    the forwarder: a worker killed mid-run
//!    (`tests/system/worker_process.rs`) and the dev benchmark end to end
//!    through the CLI (`tests/system/cli_e2e.rs`). They need
//!    `DATABASE_URL`.
//!
//! The stack is taken down afterwards, pass or fail.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const COMPOSE_FILE: &str = "deploy/compose/tools.yaml";

struct ToolServer {
    service: &'static str,
    test_service: &'static str,
    /// The relay that gives the host a way in (deploy/compose/tools.yaml).
    forwarder: &'static str,
    /// Environment variable that moves the forwarder's host port, and its
    /// default.
    host_port_env: &'static str,
    host_port: &'static str,
    /// Workspace-relative path of the egress probe.
    probe: &'static str,
    /// Reachable from inside the server container: its own MCP port.
    control: &'static str,
    /// The gateway test target for this server in `synth-tools`, and the
    /// variable that tells it the server's URL.
    gateway_test: &'static str,
    url_env: &'static str,
    /// The ignored tests of `synth-bootstrap`'s `system` target that run
    /// the binaries as processes against this server.
    process_tests: &'static [&'static str],
}

/// The packages whose binaries the process tests start.
const PROCESS_BINARIES: &[&str] = &["synth-worker", "synth-api-server", "synth-cli"];

const SERVERS: &[ToolServer] = &[ToolServer {
    service: "build123d",
    test_service: "build123d-test",
    forwarder: "build123d-forward",
    host_port_env: "BUILD123D_HOST_PORT",
    host_port: "18123",
    probe: "mcp_servers/build123d/tests/egress_probe.py",
    control: "127.0.0.1:8000",
    gateway_test: "build123d",
    url_env: "BUILD123D_URL",
    process_tests: &["worker_process", "cli_e2e"],
}];

pub fn run(root: &Path) -> Result<()> {
    ensure_docker()?;
    for server in SERVERS {
        let tested = test_server(root, server);
        let down = compose(root)
            .args(["--profile", "test", "down", "--remove-orphans"])
            .status()
            .context("failed to run docker compose down");
        tested?;
        if !down?.success() {
            bail!("docker compose down failed");
        }
        eprintln!("xtask: test-tools: {}: passed", server.service);
    }
    Ok(())
}

fn test_server(root: &Path, server: &ToolServer) -> Result<()> {
    let name = server.service;
    step(
        root,
        &format!("{name}: build"),
        &["--profile", "test", "build", name, server.test_service],
    )?;
    step(
        root,
        &format!("{name}: start"),
        &["up", "--detach", "--wait", name],
    )?;

    eprintln!("xtask: test-tools: {name}: sandbox configuration");
    let (container, networks) = inspect(root, name)?;
    let problems = sandbox_problems(&container, &networks);
    if !problems.is_empty() {
        bail!(
            "{name} is not sandboxed as {COMPOSE_FILE} requires:\n  - {}",
            problems.join("\n  - ")
        );
    }

    let forwarder = server.forwarder;
    step(
        root,
        &format!("{name}: start {forwarder}"),
        &["up", "--detach", "--wait", forwarder],
    )?;
    eprintln!("xtask: test-tools: {forwarder}: sandbox configuration");
    let (relay, relay_networks) = inspect(root, forwarder)?;
    let server_network = networks
        .first()
        .map(|(n, _)| n.as_str())
        .unwrap_or_default();
    let problems = forwarder_problems(&relay, &relay_networks, server_network);
    if !problems.is_empty() {
        bail!(
            "{forwarder} is not confined as {COMPOSE_FILE} requires:\n  - {}",
            problems.join("\n  - ")
        );
    }

    eprintln!("xtask: test-tools: {name}: egress probe inside the server container");
    let probe =
        File::open(root.join(server.probe)).with_context(|| format!("opening {}", server.probe))?;
    let output = compose(root)
        .args(["exec", "-T", name, "python", "-I", "-", server.control])
        .stdin(Stdio::from(probe))
        .output()
        .context("failed to run the egress probe")?;
    let report = String::from_utf8_lossy(&output.stdout);
    eprintln!("xtask: test-tools: {name}: {}", report.trim());
    if !output.status.success() {
        bail!(
            "{name} can reach the internet, or cannot reach its own port ({}): {}{}",
            output.status,
            report.trim(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    step(
        root,
        &format!("{name}: test suite in its container"),
        &["--profile", "test", "run", "--rm", server.test_service],
    )?;

    let port = std::env::var(server.host_port_env).unwrap_or_else(|_| server.host_port.to_owned());
    let url = format!("http://127.0.0.1:{port}/mcp");
    eprintln!("xtask: test-tools: {name}: tool gateway tests through {forwarder} ({url})");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .env(server.url_env, &url)
        .args([
            "test",
            "--locked",
            "-p",
            "synth-tools",
            "--test",
            server.gateway_test,
            "--",
            "--ignored",
        ])
        .status()
        .context("failed to run the tool gateway tests")?;
    if !status.success() {
        bail!("tool gateway tests against {name} failed ({status})");
    }

    if !server.process_tests.is_empty() {
        eprintln!("xtask: test-tools: {name}: processes through {forwarder} ({url})");
        let mut build = cargo(root);
        build.args(["build", "--locked"]);
        for package in PROCESS_BINARIES {
            build.args(["-p", package]);
        }
        let built = build.status().context("failed to build the binaries")?;
        if !built.success() {
            bail!("building the binaries failed ({built})");
        }
        let status = cargo(root)
            .env(server.url_env, &url)
            .args([
                "test",
                "--locked",
                "-p",
                "synth-bootstrap",
                "--test",
                "system",
                "--",
                "--ignored",
            ])
            .args(server.process_tests)
            .status()
            .context("failed to run the process tests")?;
        if !status.success() {
            bail!("the process tests against {name} failed ({status})");
        }
    }
    Ok(())
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root);
    command
}

/// The running container of a compose service, and each network it is
/// attached to with whether that network is internal.
fn inspect(root: &Path, service: &str) -> Result<(Value, Vec<(String, bool)>)> {
    let id = stdout(compose(root).args(["ps", "--quiet", service]))?;
    let id = id.trim();
    if id.is_empty() {
        bail!("{service} is not running");
    }
    let container = first(&stdout(docker().args(["inspect", id]))?)?;
    let mut networks = Vec::new();
    for network in attached_networks(&container) {
        let inspected = first(&stdout(docker().args(["network", "inspect", &network]))?)?;
        let internal = inspected["Internal"] == Value::Bool(true);
        networks.push((network, internal));
    }
    Ok((container, networks))
}

/// Every way the inspected forwarder falls short: it must sit on the
/// server's internal network and on exactly one other network, publish
/// exactly one container port and only on 127.0.0.1, and be hardened like a
/// server.
fn forwarder_problems(
    container: &Value,
    networks: &[(String, bool)],
    server_network: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    let names: Vec<&str> = networks.iter().map(|(n, _)| n.as_str()).collect();
    let on_server_network = networks
        .iter()
        .any(|(n, internal)| n == server_network && *internal);
    let others = networks.iter().filter(|(n, _)| n != server_network).count();
    if !on_server_network || others != 1 || networks.len() != 2 {
        problems.push(format!(
            "attached to {names:?}, not to {server_network} (internal) and one other network"
        ));
    }
    let bindings = container["HostConfig"]["PortBindings"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if bindings.len() != 1 {
        problems.push(format!(
            "publishes {} container ports, not exactly one",
            bindings.len()
        ));
    }
    for (port, hosts) in &bindings {
        let host_ips: Vec<&str> = hosts
            .as_array()
            .map(|h| h.iter().filter_map(|b| b["HostIp"].as_str()).collect())
            .unwrap_or_default();
        if host_ips.is_empty() || host_ips.iter().any(|ip| *ip != "127.0.0.1") {
            problems.push(format!(
                "publishes {port} on {host_ips:?}, not on 127.0.0.1 only"
            ));
        }
    }
    problems.extend(hardening_problems(container));
    problems
}

/// Network names the container is attached to.
fn attached_networks(container: &Value) -> Vec<String> {
    container["NetworkSettings"]["Networks"]
        .as_object()
        .map(|n| n.keys().cloned().collect())
        .unwrap_or_default()
}

/// Every way the inspected container falls short of the sandbox, given each
/// attached network and whether it is internal.
fn sandbox_problems(container: &Value, networks: &[(String, bool)]) -> Vec<String> {
    let host = &container["HostConfig"];
    let mut problems = Vec::new();
    if networks.len() != 1 {
        let names: Vec<&str> = networks.iter().map(|(n, _)| n.as_str()).collect();
        problems.push(format!("attached to {names:?}, not to one network"));
    }
    for (network, internal) in networks {
        if !internal {
            problems.push(format!(
                "network {network} is not internal, so it has egress"
            ));
        }
    }
    let published = host["PortBindings"]
        .as_object()
        .is_some_and(|bindings| !bindings.is_empty())
        || container["NetworkSettings"]["Ports"]
            .as_object()
            .is_some_and(|ports| ports.values().any(|v| !v.is_null()));
    if published {
        problems.push("publishes a port".to_owned());
    }
    problems.extend(hardening_problems(container));
    problems
}

/// Resource limits, read-only root, no capabilities, no new privileges,
/// non-root user.
fn hardening_problems(container: &Value) -> Vec<String> {
    let host = &container["HostConfig"];
    let mut problems = Vec::new();
    for (field, what) in [
        ("Memory", "memory limit"),
        ("NanoCpus", "CPU limit"),
        ("PidsLimit", "process limit"),
    ] {
        if !host[field].as_i64().is_some_and(|v| v > 0) {
            problems.push(format!("no {what} (HostConfig.{field})"));
        }
    }
    if host["ReadonlyRootfs"] != Value::Bool(true) {
        problems.push("root filesystem is writable".to_owned());
    }
    let has = |field: &str, wanted: &str| {
        host[field]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i.as_str() == Some(wanted)))
    };
    if !has("CapDrop", "ALL") {
        problems.push("capabilities are not all dropped".to_owned());
    }
    if !has("SecurityOpt", "no-new-privileges:true") && !has("SecurityOpt", "no-new-privileges") {
        problems.push("no-new-privileges is not set".to_owned());
    }
    let user = container["Config"]["User"].as_str().unwrap_or_default();
    if user.is_empty() || user == "root" || user.starts_with("0:") || user == "0" {
        problems.push("runs as root".to_owned());
    }
    problems
}

fn step(root: &Path, name: &str, args: &[&str]) -> Result<()> {
    eprintln!("xtask: test-tools: {name}");
    let status = compose(root)
        .args(args)
        .status()
        .with_context(|| format!("failed to start `{name}`"))?;
    if !status.success() {
        bail!("test-tools step `{name}` failed ({status})");
    }
    Ok(())
}

fn stdout(command: &mut Command) -> Result<String> {
    let output = command.output().context("failed to run docker")?;
    if !output.status.success() {
        bail!(
            "docker failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The first object of a `docker inspect` array.
fn first(json: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(json).context("parsing docker inspect output")?;
    value
        .get(0)
        .cloned()
        .context("docker inspect returned nothing")
}

/// The docker binary: `DOCKER` if set, else `docker` on PATH.
fn docker() -> Command {
    Command::new(std::env::var_os("DOCKER").unwrap_or_else(|| "docker".into()))
}

fn compose(root: &Path) -> Command {
    let mut cmd = docker();
    cmd.current_dir(root).args(["compose", "-f", COMPOSE_FILE]);
    cmd
}

fn ensure_docker() -> Result<()> {
    let found = docker()
        .args(["compose", "version"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        bail!("Docker with Compose is required for the tool servers' tests (see README)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sandboxed() -> Value {
        json!({
            "Config": { "User": "10001:10001" },
            "HostConfig": {
                "PortBindings": {},
                "Memory": 1_073_741_824_i64,
                "NanoCpus": 1_000_000_000_i64,
                "PidsLimit": 128,
                "ReadonlyRootfs": true,
                "CapDrop": ["ALL"],
                "SecurityOpt": ["no-new-privileges:true"]
            },
            "NetworkSettings": {
                "Ports": { "8000/tcp": null },
                "Networks": { "autosys-tools_tools": {} }
            }
        })
    }

    #[test]
    fn a_sandboxed_container_on_an_internal_network_passes() {
        let container = sandboxed();
        assert_eq!(attached_networks(&container), ["autosys-tools_tools"]);
        let problems = sandbox_problems(&container, &[("autosys-tools_tools".to_owned(), true)]);
        assert!(problems.is_empty(), "{problems:?}");
    }

    fn forwarder(bindings: Value) -> Value {
        let mut container = sandboxed();
        container["HostConfig"]["PortBindings"] = bindings;
        container
    }

    const TOOLS: &str = "autosys-tools_tools";

    #[test]
    fn a_loopback_forwarder_on_the_server_network_passes() {
        let container =
            forwarder(json!({ "8000/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "18123" }] }));
        let networks = [
            (TOOLS.to_owned(), true),
            ("autosys-tools_host_access".to_owned(), false),
        ];
        let problems = forwarder_problems(&container, &networks, TOOLS);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_forwarder_that_widens_access_is_reported() {
        let all_interfaces =
            forwarder(json!({ "8000/tcp": [{ "HostIp": "", "HostPort": "18123" }] }));
        let networks = [
            (TOOLS.to_owned(), true),
            ("autosys-tools_host_access".to_owned(), false),
        ];
        let problems = forwarder_problems(&all_interfaces, &networks, TOOLS);
        assert!(
            problems.iter().any(|p| p.contains("127.0.0.1 only")),
            "{problems:?}"
        );

        let two_ports = forwarder(json!({
            "8000/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "18123" }],
            "9000/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "19000" }]
        }));
        let problems = forwarder_problems(&two_ports, &networks, TOOLS);
        assert!(
            problems.iter().any(|p| p.contains("not exactly one")),
            "{problems:?}"
        );

        let loopback =
            forwarder(json!({ "8000/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "18123" }] }));
        let extra_network = [
            (TOOLS.to_owned(), true),
            ("autosys-tools_host_access".to_owned(), false),
            ("bridge".to_owned(), false),
        ];
        assert!(!forwarder_problems(&loopback, &extra_network, TOOLS).is_empty());
        let off_server_network = [("autosys-tools_host_access".to_owned(), false)];
        assert!(!forwarder_problems(&loopback, &off_server_network, TOOLS).is_empty());
    }

    #[test]
    fn every_gap_in_the_sandbox_is_reported() {
        let container = json!({
            "Config": { "User": "" },
            "HostConfig": {
                "PortBindings": { "8000/tcp": [{ "HostIp": "", "HostPort": "8000" }] },
                "Memory": 0,
                "NanoCpus": 0,
                "PidsLimit": null,
                "ReadonlyRootfs": false,
                "CapDrop": null,
                "SecurityOpt": null
            },
            "NetworkSettings": { "Ports": {}, "Networks": {} }
        });
        let networks = [("bridge".to_owned(), false), ("tools".to_owned(), true)];
        let problems = sandbox_problems(&container, &networks);
        assert_eq!(problems.len(), 10, "{problems:#?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("bridge is not internal"))
        );
    }
}
