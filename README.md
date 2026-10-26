# autosys

Agentic system-synthesis platform: requirements in, verified design artifacts out. A Rust harness runs an orchestrator agent that calls engineering tools (Python and Julia MCP servers in containers) and records the design in the SDR (Structured Design Representation).

The repository is built in increments. The current plan is [Increment 1: Execution Kernel](docs/plans/increment-1.md). Crates and folders appear only when the PR that needs them lands.

## Prerequisites

- [rustup](https://rustup.rs). The toolchain version is pinned in `rust-toolchain.toml`, and rustup installs it on first use. On Windows, rustup needs either the MSVC C++ build tools, or the `x86_64-pc-windows-gnu` host with a full MinGW-w64 toolchain on `PATH` (for example WinLibs: `winget install BrechtSanders.WinLibs.POSIX.UCRT`), because the toolchain's bundled `dlltool` cannot run alone.
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), installed with `cargo install --locked cargo-deny`.
- [sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli) 0.9.0, installed with `cargo install --locked sqlx-cli --version 0.9.0 --no-default-features --features postgres`.
- Docker with Compose, to run PostgreSQL (see [Database](#database)) and the tool servers (see [Tool servers](#tool-servers)).
- [uv](https://docs.astral.sh/uv/) 0.11.26, to generate the tool servers' Python models. If `uv` is not on `PATH`, point the `UV` environment variable at its binary.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

Before opening a pull request, run every check CI runs:

```sh
cargo xtask ci
```

It needs Postgres running and `DATABASE_URL` set (see [Database](#database)), uv, and Docker. It runs, in order:

1. `cargo fmt --check`;
2. `cargo clippy`, with warnings as errors;
3. `cargo xtask arch-check`;
4. `cargo xtask validate-config`, which checks every file in `config/` against its schema (see [Configuration](#configuration));
5. `cargo xtask validate-benchmarks`, which checks every problem in `evals/benchmarks/` (see [Benchmarks](#benchmarks));
6. `cargo xtask codegen --check`, which fails if the tool servers' generated models differ from `schemas/tools/`;
7. `cargo sqlx database setup`, which creates the database at `DATABASE_URL` if needed and applies the migrations;
8. `cargo sqlx prepare --check`, which fails if the query data in `.sqlx/` is out of date;
9. `cargo test`;
10. `cargo xtask test-tools`, which runs each tool server's tests in its container, checks its sandbox and its forwarder, and runs the tool gateway's tests against it (see [Tool servers](#tool-servers));
11. `cargo deny check`.

Clippy and the tests build with `SQLX_OFFLINE=true`, so they use the committed query data rather than the live database. CI starts Postgres from the same compose file and then runs only `cargo xtask ci`, so a local pass means a CI pass.

`cargo xtask arch-check` fails when a crate depends on a workspace crate that blueprint §10.3 does not allow, when `synth-testkit` is used as anything but a dev-dependency, or when a crate other than `contracts`, `testkit`, `sdr`, `infra` or `bootstrap` names `SdrRepository` (§7.2). It also limits lease fencing (ADR-0032): only `contracts`, `testkit` and `infra` may call `FenceToken::issue_for_claim`; only `contracts`, `testkit`, `bootstrap` and `runtime/src/scheduler/` may name `authorized_by` or `UnfencedReason`, so other crates' tests get write authority from `testkit::leases`; and only `tools/src/gateway.rs` and `testkit` may call `.invoke(` on a tool. Paths are compared by component, so the rules hold on Windows too.

## Implementing a contract

The traits every harness crate implements live in `crates/contracts`. `crates/testkit` has an in-memory implementation or fake for each, and one contract-test suite per trait in `testkit::contract`. A new implementation (for example a Postgres store) runs the same suite from its own tests, with `synth-testkit` as a dev-dependency.

## Database

Jobs, events, SDR versions, evidence and artifact metadata are stored in PostgreSQL ([ADR-0016](docs/adr/0016-postgres-persistence.md)); artifact files go on a mounted volume. The stores live in `crates/infra`, and every timestamp they set, lease expiry included, comes from the database clock ([ADR-0023](docs/adr/0023-lease-times-from-database-clock.md)). Every write is fenced ([ADR-0032](docs/adr/0032-lease-fencing.md)): it carries the fence from its job's claim, checked in the write's own transaction, or one of four unfenced reasons the store allows, and a worker whose job was reclaimed gets `lease_lost`.

To start a local database:

1. Copy `.env.example` to `.env`, which Git ignores. Choose a password for `AUTOSYS_DB_PASSWORD` and use the same one in `DATABASE_URL`.
2. Start Postgres. It listens on 127.0.0.1 only.

   ```sh
   docker compose --env-file .env -f deploy/compose/compose.yaml up -d --wait
   ```

3. Export both variables in your shell before building or testing. Test binaries do not read `.env`.

Migrations live in `crates/infra/migrations/` and are embedded in the binary. A merged migration never changes; add a new one instead.

`sqlx` checks every query at compile time: against the database at `DATABASE_URL` when it is set, and against the committed `.sqlx/` data when `SQLX_OFFLINE=true`. After changing a query or a migration, run `cargo sqlx database setup --source crates/infra/migrations`, then `cargo sqlx prepare --workspace`, and commit `.sqlx/`.

The PostgreSQL stores sit behind `synth-infra`'s default `postgres` feature. Code that only loads configuration, such as xtask, turns it off, so building it needs no database.

The store tests in `crates/infra/tests/` create a database named `autosys_test_*` on the `DATABASE_URL` server for each test and drop it at the end. A test that panics leaves its database behind; drop it by hand. Without `DATABASE_URL` these tests fail; they never skip.

## Design state (SDR)

`crates/sdr` exports `SdrManager`, the only writer of design state (blueprint §7.2); its storage is crate-private. A change is proposed, validated, then committed or rejected, and the types make committing an unvalidated transition a compile error.

`validate` checks the transition against `schemas/sdr/transition.schema.json`, including constraints the Rust types do not enforce, such as `minItems`. It then requires the transition's base to be the current head, applies the operations, and checks the result against `system.schema.json`. Problems come back as typed reasons, not errors. Versions are linear: a transition on a stale base is rejected, never rebased ([ADR-0020](docs/adr/0020-sdr-v0-scope.md)).

## Checks and gates

Checks produce evidence; gates decide (blueprint §15).

- **Checks** (`crates/evaluation`) are deterministic and report `pass`, `fail`, `unverified` or `error`:
  - `schema`: the document satisfies its JSON Schema;
  - `units`: every unit is a supported UCUM code, and each numeric requirement uses the dimension of the parameter it constrains;
  - `requirement.numeric_threshold`: compares a parameter with a requirement's threshold after unit conversion ([ADR-0021](docs/adr/0021-numeric-requirement-check-v0.md)). A requirement it cannot decide is `unverified`, never `pass`.
- **The `Evaluator`** stores each finding as evidence and records a `check_completed` event.
- **Gates** (`crates/governance`):
  - the tool permission gate allows only tools listed in `config/tools/`;
  - the SDR commit gate rejects a transition when a check required by `config/policies/gates.yaml` failed, and attaches that evidence;
  - the write-scope gate denies a transition that changes an element outside its task's SDR write scope;
  - the approval gate asks a human when `config/policies/autonomy.yaml` requires it.
- **The `GateRunner`** writes every decision to the event log as `gate_decided`; the most restrictive decision wins.
- **Hooks** observe every event through `ObservedEventLog` and can never stop the run.

## Configuration

`config/` holds non-secret declarative configuration in YAML. The loader in `crates/infra` parses each file and validates it against its schema in `schemas/config/` before deserializing it, so every error names the file and the path of the problem.

YAML is parsed with [serde-saphyr](https://crates.io/crates/serde-saphyr):
- a pure-Rust parser with limits against alias bombs;
- YAML 1.2 booleans, so `no` stays a string;
- duplicate keys, merge keys and custom tags are rejected.

Run `cargo xtask validate-config` to check `config/` without starting anything. These are all errors:

- a file in a location with no schema;
- a tool overlay whose `request_schema` or `result_schema` names no file in `schemas/`;
- a model routing file the gateway would reject;
- an agent whose model profile is missing, or has no tool calling while the agent has tools;
- a context profile whose total is above the ceiling in `config/policies/context.yaml`, whose fixed sections are capped above its own total, or a missing `default` profile;
- a prompt in `prompts/` with no header, an invalid header, or an id another prompt has;
- an agent in `config/agents/` whose prompt is not in `prompts/`, whose tools are neither `sdr_read`, `sdr_propose` nor listed in `config/tools/`, that names a skill, whose result schema does not exist, or that escalates when its repairs run out.

## Tool servers

Engineering tools are MCP servers in `mcp_servers/`, one container each (blueprint §3.2). Each is self-contained: its own `pyproject.toml`, `uv.lock`, Dockerfile and tests. It holds no harness logic, and no tool accepts code to run. Tools are listed for the harness in `config/tools/<server>.yaml`; a tool missing there is unavailable.

- **[build123d](mcp_servers/build123d/README.md)** has two tools, `create_plate` and `measure`. Their request and result schemas are in `schemas/tools/build123d/`.
- **Generated models.** `cargo xtask codegen` writes each server's Python models (pydantic) and self-contained copies of its schemas to `mcp_servers/<server>/models/`. Never edit them; change the schema and regenerate. The generator, datamodel-code-generator, is pinned in the server's `uv.lock`.
- **Sandbox.** `deploy/compose/tools.yaml` runs the servers on an internal Docker network: no internet egress and no published ports. They run as non-root with a read-only root filesystem, no capabilities, and CPU, memory and process limits. MCP runs over streamable HTTP inside that network.
- **Reaching a server from the host.** Each server has a forwarder, for example `build123d-forward`: a TCP relay (HAProxy) with one fixed backend. It is on the server's internal network and on `host_access`, and publishes one port on 127.0.0.1 only (`BUILD123D_HOST_PORT`, default 18123). The server's own networks do not change. Start it with `docker compose -f deploy/compose/tools.yaml up -d --wait build123d-forward`; the MCP endpoint is then `http://127.0.0.1:18123/mcp`.
- **`cargo xtask test-tools`** builds each server's images and starts it, then:
  1. checks the sandbox with `docker inspect`;
  2. starts the forwarder and checks it: on the server's network and one other, one port on 127.0.0.1 only, the same hardening;
  3. runs an egress probe inside the server container, with the forwarder up, which must reach nothing on the internet;
  4. runs the server's test container: tests, ruff and `mypy --strict`;
  5. runs the tool gateway's tests in `crates/tools/tests/` against the server, through the forwarder;
  6. builds `worker`, `api-server` and `synth`, and runs the process tests through the forwarder: a worker killed mid-run, and the dev benchmark end to end through the CLI.

## Tool gateway

`crates/tools` is the harness side of every tool call (blueprint §14.2). `McpServer` connects with rmcp, pinned to MCP protocol 2026-07-28, and lists the server's tools. `ToolRegistry` keeps the tools that a `config/tools/` overlay lists. `ToolGateway` runs a call through these steps: replay from its step record, validation against the request schema, the tool permission and approval gates, execution with the overlay's timeout and the step's cancellation, validation of the result, evidence, and the step record. A tool error, timeout or invalid result comes back as `ToolObservation::Failed`, never as success. ADR-0026 has the replay rules.

The gateway tests that need the real server are ignored by a plain `cargo test`; `cargo xtask test-tools` runs them. To run them by hand:

```sh
docker compose -f deploy/compose/tools.yaml up -d --wait build123d-forward
BUILD123D_URL=http://127.0.0.1:18123/mcp cargo test -p synth-tools --test build123d -- --ignored
```

To work on a server without Docker, run its tests with uv from its directory:

```sh
cd mcp_servers/build123d
uv sync --frozen
uv run --frozen pytest
```

The isolation tests skip outside the compose network; only `cargo xtask test-tools` runs them.

## Model gateway

`crates/model-gateway` is the harness side of every model call (blueprint §14.4, [ADR-0027](docs/adr/0027-model-gateway-providers.md), [ADR-0028](docs/adr/0028-pluggable-model-providers.md)). This branch is the Pliny-only variant ([ADR-0036](docs/adr/0036-pliny-only-variant.md)): one provider is registered behind `contracts::ModelProvider`, `openai_compatible`, which serves the internal Pliny platform. No Claude model is used.

Profiles are in `config/models/routing.yaml`. Each one names a provider, a model and the shared limits. Its `settings` follow the provider's schema in `schemas/config/model-providers/`.

- **Choosing a model.** Each profile pairs a provider with one model. `pliny` is the only profile and the default. `SYNTH_MODEL_PROFILE` picks another profile for a run, and an agent that names a profile in `model_profile` keeps it. Another model served by Pliny is one more profile in the routing file; no code changes.
- **Tool calling.** `pliny` has `tool_calling: true`. `ModelGateway::start` refuses a profile with `tool_calling: false` for an agent with tools.
- **Credentials.** Keys and the Pliny base URL come from environment variables (see `.env.example`). A missing one fails at startup, naming it.
- **TLS.** Verification always uses the system's roots. `INTERNAL_CA_BUNDLE` adds a PEM bundle of internal CAs.
- **Retries.** Rate limits, overload and timeouts are retried with a bound. Every request has a timeout and stops when its context is cancelled.
- **Cost.** Each call reports its tokens and its cost at the prices in the routing file. Pliny's model has no price, so its cost is reported as unknown, never as zero.

Each provider is one module, `crates/model-gateway/src/providers/<provider>/`: its adapter, its factory, its tests, its recordings and a live smoke test.

**To remove a provider:**
1. Delete its module.
2. Delete `schemas/config/model-providers/<provider>.schema.json` and its examples.
3. Remove its two lines in `providers/mod.rs` (`pub mod`, and its entry in `default_registry()`), and its `ROOT_TYPES` entry in `crates/schema-types/tests/schemas.rs`.
4. Remove its profiles and prices from `routing.yaml` and its variables from `.env.example`. `cargo xtask validate-config` names any profile left on an unregistered provider.

To add a provider, do the same in reverse, plus its adapter.

The tests replay hand-written provider responses from a loopback server, so CI needs no key. The smoke tests against Pliny are ignored by default; they need `PLINY_API_KEY` and `PLINY_BASE_URL`, and `SYNTH_MODEL_PROFILE` picks the profile:

```sh
cargo test -p synth-model-gateway --lib -- --ignored --test-threads=1
```

## Context

`crates/context` decides what each model call sees (blueprint §8, [ADR-0029](docs/adr/0029-context-assembly-v0.md)). A context is built fresh for every call from the task contract, the job's state, the SDR slice and the run's recent events. Nothing accumulates, and no model summarises anything. It reads the SDR only through `SdrManager::head` and `SdrManager::read`.

- **Sections.** Seven, always in this order: `instructions` (the system prompt: `prompts/system/base.md`, then the agent's prompt), `task_contract`, `job_state`, `design_state`, `run_history`, `untrusted_content` and `open_turn`.
- **Trust.** Text that did not come from `prompts/` is escaped, so no input can close a section or forge a label. Records the harness built are labelled `data`. Free text from events and tool results is labelled `untrusted` and wrapped in `<untrusted>` envelopes. Only `ToolResult` content is wrapped; the model's own blocks, thinking included, pass through unchanged.
- **Budgets.** `config/context.yaml` holds named profiles: a total, a cap per section, and `bytes_per_token`, the divisor of the size estimate (there is no tokenizer). `config/policies/context.yaml` holds the ceiling no profile may exceed. The improvement loop may rebalance or lower profiles within it, never raise it.
- **When it does not fit.** The task, the job state, the design and the open turn are never cut: one that does not fit fails the build with `context_overflow`. Run history and untrusted text are trimmed, untrusted first, oldest first, and a line says what was left out.
- **The hash.** `AssembledContext::hash` covers exactly what the model sees. `ContextBuilder::build` records it in a `context_assembled` event with the SDR version and the last event it saw, so the same context can be rebuilt with `BuildRequest::at`. The caller must not append that event itself.
- **Prompts.** Static Markdown with a YAML header, in [prompts/](prompts/README.md). Each header is checked against `schemas/prompts/prompt-header.schema.json`. A run records each prompt's id, version and hash.
- **Golden tests.** `tests/golden/context/` pins the assembled context for fixed inputs, so a change to a prompt shows up as a diff. After an intended change, run `UPDATE_GOLDEN=1 cargo test -p synth-context --test golden` and read each changed file before committing it.

## Query loop

`crates/runtime` runs a task with its agent ([ADR-0030](docs/adr/0030-query-loop-v0.md)). `Orchestrator::run` takes a job and returns a stream of the run's events, then how it stopped. Every component is passed in at startup; the runtime reads no configuration itself.

- **Agents.** `AgentRegistry` holds the specs in `config/agents/`. It refuses one whose prompt is not in `prompts/`, whose tools nobody offers, that names a skill, or that would escalate. A task may narrow its agent's tools and limits, never widen them. `config/agents/main_orchestrator.yaml` is the one agent of Increment 1.
- **The loop.** Each step builds the context, calls the model, then runs the reply's tool calls in order. A turn closes when the model replies without a tool call or when one of its proposals commits.
- **The design tools.** `sdr_read` and `sdr_propose` run in process, through `SdrManager`. A proposal is validated, checked, and then decided by the commit gates: the `sdr_commit` gate, the approval gate and the task's write-scope gate. Only `allow` commits. Every other tool goes through the tool gateway. Each tool's definition is its request schema in `schemas/tools/`, and each result follows `schemas/tools/tool-result.schema.json`.
- **Stop reasons** (blueprint §5.4):
  - `success`: only right after a final commit whose checks pass for every required requirement. What the model says never counts.
  - `failure`: one failed call, rejected proposal or reply without success beyond `max_repair_attempts`.
  - `budget_exhausted`: tokens, tool calls, steps or wall time used up, or a context that does not fit.
  - `cancelled`: the run's context was cancelled.
  - `approval_required`: a gate asked a human. The outcome carries a `Continuation`, and `Orchestrator::resume` continues it with the answer.
- **The stream.** It yields every event of the run, read back from the event log in order, then the outcome. It is driven by polling, with no spawned task. A harness failure ends it with an error instead, and no `run_stopped` is written.
- **Timeouts.** `config/runtime.yaml` holds the timeout of the loop's own reads and writes.

`cargo test -p synth-runtime` runs every stop reason with a scripted model and fake build123d tools. The ignored live test checks that Pliny accepts a context rebuilt in the middle of a tool-use turn. It needs `PLINY_API_KEY` and `PLINY_BASE_URL`:

```sh
cargo test -p synth-runtime --test live -- --ignored
```

## Worker

The `worker` binary (`apps/worker`) claims jobs and runs them; a run outlives any one process ([ADR-0031](docs/adr/0031-scheduler-and-recovery-v0.md)). `crates/bootstrap` builds it from `config/`, `prompts/` and the environment, after startup checks that stop it with exit code 2.

```sh
cargo run -p synth-worker -- --config config --prompts prompts --id worker-1
```

- **Environment.** `DATABASE_URL` and the model keys `config/models/routing.yaml` names are required. `SYNTH_ENVIRONMENT` is `local`, `test` or `production`; unset means production. `SYNTH_WORKER_ID` names the worker unless `--id` does. `RUST_LOG` filters the log on stderr, `info` by default. No value is ever logged.
- **Settings.** `config/worker.yaml` holds the lease, heartbeat, poll and retry intervals, the pool, and the tool servers to connect to. Short intervals are allowed only in the test environment.
- **Jobs.** A job moves through queued, running, suspended (waiting for an approval), and completed, cancelled or failed. Every change is a `job_state_changed` event. A retryable error sends a job back to the queue with a growing delay, up to `max_attempts`.
- **Crashes.** The run is checkpointed at every phase boundary. After a crash, another worker claims the job once its lease expires and continues from the checkpoint. A tool call or commit never happens twice: a call to a tool that is not idempotent and was interrupted comes back as `outcome_unknown`.
- **Shutdown.** Ctrl-C, SIGTERM or Ctrl-Break gives the running job back to the queue and exits with 0. A second signal exits at once.

`cargo test -p synth-bootstrap --test system` runs every crash point and the worker scenarios on Postgres, with a scripted model and fake tools; it needs `DATABASE_URL`. `cargo xtask test-tools` also kills a real worker process mid-run against the build123d server.

## API server

The `api-server` binary (`apps/api-server`) serves the external HTTP API v0 that `schemas/api/openapi.yaml` describes ([ADR-0018](docs/adr/0018-schema-first-api-v0.md)). Clients start runs, stream their events, answer approvals, cancel, and read reports and artifacts; the worker does the running.

```sh
SYNTH_API_TOKEN=<a long random string> cargo run -p synth-api-server -- --config config --prompts prompts
```

- **Environment.** `DATABASE_URL` and `SYNTH_API_TOKEN` are required; without the token the server does not start (exit code 2). Every request sends it as `Authorization: Bearer <token>` ([ADR-0022](docs/adr/0022-dev-token-auth-increment-1.md)): it suits a developer's machine only. `SYNTH_ENVIRONMENT` and `RUST_LOG` work as for the worker.
- **Settings.** `config/api.yaml` holds the listen address (loopback), the request time limit and body limit, the event stream's keep-alive and page size, the pool, and the artifact volume.
- **Runs.** `POST /v0/runs` takes a brief and the requirements, with an `Idempotency-Key`: the same key and request return the same run. The requirements become the project's SDR version 0.
- **Events.** `GET /v0/runs/{run_id}/events` streams the run's log as server-sent events. A client that reconnects with `Last-Event-ID` receives every later event exactly once. The stream ends once the run has stopped for good.

`cargo test -p synth-api` checks every documented response against the OpenAPI document, in memory.

## CLI

The `synth` binary (`apps/cli`) is a thin client of the API server, built on `crates/api-client` ([ADR-0034](docs/adr/0034-cli-and-api-client.md)). It sends requests and prints the server's JSON; the server decides everything.

```sh
cargo xtask benchmark-request part_design_001 > request.json
synth run start request.json
synth run watch <run_id>
synth run approve <run_id> <approval_id> --answer approve
synth run report <run_id>
```

- **Environment.** `SYNTH_API_URL` (default `http://127.0.0.1:8080`) and `SYNTH_API_TOKEN`, which has no flag.
- **Watching.** `watch` journals every event under `runtime-data/cli/runs/<run_id>/` before printing it, so run again it resumes after the last event it received, with no gaps or repeats.
- **Benchmarks.** `cargo xtask benchmark-request <id>` builds a dev problem's `RunRequest` with `evaluation::benchmarks::run_request`. Notes and expected outcomes never reach the run.

`cargo xtask test-tools` runs the dev benchmark end to end through `synth`, `api-server` and two `worker` processes, one of them killed mid-run. To run the stack yourself, follow [Running a design run locally](docs/developer-guide/running-locally.md).

## Benchmarks

Benchmark problems live in `evals/benchmarks/<split>/<id>/problem.yaml` and follow `schemas/benchmarks/benchmark-problem.schema.json` ([ADR-0025](docs/adr/0025-benchmark-problem-format.md)). Only `dev` problems are stored in the repository. The team writes them; values come from the specification each file cites in `source`, unchanged.

Each problem holds:
- requirements, using the numeric requirement check's criteria;
- budgets;
- expected outcomes, each with the specification's wording and typed checks an evaluation decides from evidence.

A mass or other reference value is an arithmetic expression over parameter IDs. It is parsed, never executed.

`cargo xtask validate-benchmarks` checks:
- each file against the schema;
- that identifiers are unique and every reference names something the problem declares;
- that each problem's location matches its `split` and `id`;
- that approval outcomes name rules in `config/policies/autonomy.yaml`.

## Schemas

`schemas/` holds the canonical contracts as JSON Schema (draft 2020-12). Rust types are generated from them at build time by `crates/schema-types`; never write or edit those types by hand. To change a type, change its schema.

- Each `*.schema.json` file has at most one root type, named by its `title`. Type names must be unique across all schema files.
- Examples live in `schemas/<area>/examples/<stem>[.<case>].json` and must validate against `<area>/<stem>.schema.json`. Examples that must be rejected go in `examples/invalid/`.
- `cargo test -p synth-schema-types` checks every schema against the 2020-12 metaschema, validates every example, and round-trips each valid example through its Rust type.
- `schema_types::SCHEMA_FILES` embeds every schema file at build time, so the harness validates values against the same schemas its types were generated from.
- Units are [UCUM](https://ucum.org/) case-sensitive codes, for example `kg`, `mm`, `mm3` and `kg/m3`.

## Documentation

- [Engineering standards and repository blueprint](docs/architecture/blueprint.md) (normative)
- [Increment 1 plan](docs/plans/increment-1.md)
- [Running a design run locally](docs/developer-guide/running-locally.md)
- [CLAUDE.md](CLAUDE.md): working rules for coding agents

### Architecture decisions

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](docs/adr/0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](docs/adr/0002-rust-harness-python-julia-tools.md) | Rust for the harness; Python and Julia for tool workers | Accepted |
| [0015](docs/adr/0015-async-trait-convention.md) | Async methods on trait objects use `async-trait` | Accepted |
| [0016](docs/adr/0016-postgres-persistence.md) | PostgreSQL for jobs, events and design state | Accepted |
| [0018](docs/adr/0018-schema-first-api-v0.md) | Schema-first external API v0 | Proposed |
| [0019](docs/adr/0019-retain-frontend-rebuild-rest.md) | Retain the frontend; rebuild the rest | Accepted |
| [0020](docs/adr/0020-sdr-v0-scope.md) | SDR v0 scope | Accepted |
| [0021](docs/adr/0021-numeric-requirement-check-v0.md) | Numeric requirement check for Increment 1 | Accepted |
| [0022](docs/adr/0022-dev-token-auth-increment-1.md) | Development-token authentication for Increment 1 | Accepted |
| [0023](docs/adr/0023-lease-times-from-database-clock.md) | Lease times come from the database clock | Accepted |
| [0024](docs/adr/0024-schema-validation-in-contracts.md) | Validation against the canonical schemas lives in contracts | Accepted |
| [0025](docs/adr/0025-benchmark-problem-format.md) | Benchmark problem format v0 | Proposed |
| [0026](docs/adr/0026-step-records-and-replay.md) | Step records and replay of tool calls | Proposed |
| [0027](docs/adr/0027-model-gateway-providers.md) | Model gateway with two providers, Claude by default | Proposed; profile format superseded by 0028 |
| [0028](docs/adr/0028-pluggable-model-providers.md) | Pluggable model providers | Proposed |
| [0029](docs/adr/0029-context-assembly-v0.md) | Context assembly v0 | Proposed |
| [0030](docs/adr/0030-query-loop-v0.md) | Query loop v0 | Proposed |
| [0031](docs/adr/0031-scheduler-and-recovery-v0.md) | Scheduler and recovery v0 | Proposed |
| [0032](docs/adr/0032-lease-fencing.md) | Lease fencing | Proposed |
| [0033](docs/adr/0033-run-report-v0.md) | Run report v0 | Proposed |
| [0034](docs/adr/0034-cli-and-api-client.md) | The CLI, the API client and benchmark run requests | Proposed |
| [0036](docs/adr/0036-pliny-only-variant.md) | A Pliny-only variant of the harness (this branch) | Proposed |

## License

[MIT](LICENSE)
