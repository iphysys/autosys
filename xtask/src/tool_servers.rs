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
//! 3. runs the server's test container on the same network: tests (the same
//!    probes from a sibling, with the server as the control), lint and
//!    strict type check.
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
    /// Workspace-relative path of the egress probe.
    probe: &'static str,
    /// Reachable from inside the server container: its own MCP port.
    control: &'static str,
}

const SERVERS: &[ToolServer] = &[ToolServer {
    service: "build123d",
    test_service: "build123d-test",
    probe: "mcp_servers/build123d/tests/egress_probe.py",
    control: "127.0.0.1:8000",
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
    let id = stdout(compose(root).args(["ps", "--quiet", name]))?;
    let id = id.trim();
    if id.is_empty() {
        bail!("{name} is not running");
    }
    let container = first(&stdout(docker().args(["inspect", id]))?)?;
    let networks = attached_networks(&container);
    let mut internal = Vec::new();
    for network in &networks {
        let inspected = first(&stdout(docker().args(["network", "inspect", network]))?)?;
        internal.push((network.clone(), inspected["Internal"] == Value::Bool(true)));
    }
    let problems = sandbox_problems(&container, &internal);
    if !problems.is_empty() {
        bail!(
            "{name} is not sandboxed as {COMPOSE_FILE} requires:\n  - {}",
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
    )
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
