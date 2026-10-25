# Increment 1 Plan: Execution Kernel

Oct 22, 2026 · Akshay

## Scope and exit criterion

Increment 1 delivers the execution kernel in 15 PRs: one benchmark problem runs end to end from the CLI, survives a worker kill and a CLI reconnect, and produces its run report.

- **Backend only.** The frontend and BFF are out of scope. The CLI is a thin HTTP client of `api-server` and uses the same contract (`schemas/api/openapi.yaml`) the frontend will use later, so integration adds a second client rather than reworking the backend.
- **One tool server.** A build123d MCP server with typed parametric tools, ported from `legacy/build123d_tool.py` into `mcp_servers/build123d/`.
- **Only what this increment uses gets created.** No empty crates for later increments (blueprint §10.1).

**Done when:**

1. `synth run start` on the dev benchmark problem ends with a stop reason decided by checks and gates, not by the model.
2. Killing the worker mid-run, then restarting it, resumes the run from the job store. No tool call or SDR commit is repeated.
3. Interrupting `synth run watch` and running it again resumes the event stream from the last sequence number, with no gaps or duplicates.
4. An approval required by the autonomy policy is answered with `synth run approve`, and the run continues.
5. `synth run report` shows the run report, and every requirement status links to its evidence.
6. `cargo xtask eval` runs the benchmark repeatedly and records the results as the single-loop baseline.

## Decisions before PR 1

Eight choices shape the first commits, so the team settles them before PR 1. Each has a proposed default, and decisions 1, 2, 4, 5, 6 and 8 are recorded as ADRs in `docs/adr/`.

| # | Decision | Proposed default | Why |
| --- | --- | --- | --- |
| 1 | Persistence backend (ADR-0016) | PostgreSQL through `sqlx`, migrations in the repo; artifact files on a mounted volume | `api-server` and `worker` are separate processes. Postgres gives job claiming with `FOR UPDATE SKIP LOCKED`, an append-only event log with sequence numbers, and `LISTEN/NOTIFY` to push new events and wake waiting jobs. Repository traits keep it swappable. |
| 2 | Async trait convention (ADR-0015) | `async-trait` for traits held as trait objects (tools, model providers, gates, checks, stores); native `async fn` for generic-only traits | One consistent pattern that is object-safe today. |
| 3 | Benchmark problem | One small part-design problem solved with build123d through typed parametric tools | The tool code exists in `legacy/`. Typed operations mean no agent-written code, so Increment 1 needs container isolation only. Agent-written CAD scripts need a code-execution sandbox and move to Increment 2. RCAIDE waits until a sizing problem needs it. |
| 4 | Model providers (ADR-0027) | Two adapters: Claude through the Messages API, the default profile; and the internal Pliny platform through its OpenAI-compatible endpoint, which has no tool calling yet and is refused at startup for agents with tools. Recorded responses in CI | The team uses both. A `tool_calling` flag per profile keeps a model that cannot call tools away from agents that need them. |
| 5 | SDR v0 scope (ADR-0020) | Requirements with numeric thresholds and units, parameters with units and source, decisions with rationale, assumptions, evidence links. Linear versions, no branches; one committed transition is one version. | Smallest model that lets checks decide requirement status. Branches arrive in Increment 2. |
| 6 | Requirement check v0 (ADR-0021) | Numeric threshold checks on SDR quantities; formalized requirements with a solver stay in Increment 3 | The `success` stop reason needs evidence-backed requirement status. This is an addition to blueprint §24. |
| 7 | What needs approval | The autonomy policy asks before the final design commit | Exercises the ask, suspend and resume path once. |
| 8 | API authentication (ADR-0022) | One development token from an environment variable | Real authentication is Open Decision 9 and waits for the frontend. |

The benchmark problem's requirements (3–5 must-level numeric requirements with units, such as a mass limit and an envelope) come from the team, not from this plan.

## What runs

```text
  CLI (synth)  -- commands (HTTP) -->  api-server  <------------>  PostgreSQL
  thin client  <-- events (SSE) -----  OpenAPI v0                  jobs, leases, approvals
  no logic                             resume by event ID          append-only event log
                                                                   SDR versions, evidence
                                                                        ^
                                                                        | claim jobs,
                                                                        | append events
                                                                        v
  Model providers <---- HTTPS ----------------------------------  worker
  Claude, Pliny                                                  query loop, scheduler
  recorded in CI          build123d MCP server  <---- MCP -----  context, gates, SDR
                          container, no egress                   tool and model gateways
                          typed, idempotent tools
```

The CLI only talks to `api-server`. The worker claims jobs from Postgres, calls the build123d tool server inside the compose network and the model over HTTPS, and every event it writes reaches the CLI through the resumable stream. Postgres and the tool server run in Docker Compose; `api-server` and `worker` run with `cargo run`.

## PR overview

Fifteen PRs in four tracks. The critical path is PR 1 → 2 → 3 → 5 → 6 → 8 → 11 → 12a → 12 → 15; everything else runs beside it once PR 3 lands.

| PR | Title | Track | Depends on | Size |
| --- | --- | --- | --- | --- |
| 1 | Workspace, CI and ADRs | Core | none | S |
| 2 | Schemas v0 and type generation | Core | 1 | M |
| 3 | Contracts and testkit | Core | 2 | M |
| 4 | Postgres stores and dev compose | Core | 3 | M |
| 5 | SDR v0 | Core | 3 | M |
| 6 | Checks, gates and approvals | Core | 3, 5 | M |
| 7 | build123d tool server and dev benchmark | Tools | 2 | M |
| 8 | Tool gateway | Tools | 3, 6, 7 | M |
| 9 | Model gateway | Agent | 3 | M |
| 10 | Context v0 | Agent | 3, 5 | M |
| 11 | Query loop and orchestrator | Agent | 6, 8, 9, 10 | L |
| 12a | Lease fencing | Agent | 4, 11 | M |
| 12 | Scheduler, worker and recovery | Agent | 4, 11, 12a | L |
| 13 | Run report and artifacts | Core | 5, 6 | S |
| 14 | API v0 and api-server | Interface | 4, 13 | M |
| 15 | CLI, end-to-end run and baseline | Interface | 12, 14 | M |

PR 7 can start as soon as PR 2 lands, since porting the build123d tool needs only its tool schemas. Every PR leaves CI green and follows the commit and PR rules in blueprint §19.

## PR details

Each PR lists what it builds and the tests that prove it is done.

### PR 1 — Workspace, CI and ADRs

An empty repository whose rules are already enforced.

- Cargo workspace with `[workspace.dependencies]` and `[workspace.lints]` (`unsafe_code` forbidden, `unwrap` denied outside tests), `rust-toolchain.toml`, `deny.toml`, `.gitignore` covering `runtime-data/`.
- `xtask` with `ci`: format, clippy with warnings as errors, tests, `cargo-deny`. The CI pipeline only calls `cargo xtask ci`.
- `README.md` with build and test instructions, linking the ADRs already in `docs/adr/`; `AGENTS.md` as a one-line pointer to `CLAUDE.md`.
- Tag the old repository `legacy-final` for reference.

**Done when:** CI is green on the empty workspace and fails on a planted clippy warning.

### PR 2 — Schemas v0 and type generation

The contracts every language generates from.

- JSON Schemas for task, result, event (with a per-run sequence number), job (states, lease, wake condition), SDR v0 (decision 5), transition, run report, and config (agent spec, tool overlay, policies).
- A skeleton `schemas/api/openapi.yaml`, filled in PR 14.
- `crates/schema-types` generating Rust types with typify in `build.rs`; `xtask codegen` for the other languages, starting in PR 7.
- Example instances for each schema, validated in CI.

**Done when:** the generated types compile and every example instance validates.

### PR 3 — Contracts and testkit

The traits every later crate implements or uses, with fakes for each.

- `crates/contracts`: newtype IDs; `ExecutionContext` with a cancellation token and correlation IDs; an error enum with stable codes and a retryable flag.
- Traits: `Tool`, `ModelProvider`, `Check`, `Gate`, an event subscriber for hooks, and the stores (`JobStore`, `EventLog`, `SdrRepository`, `EvidenceStore`, `ArtifactStore`).
- `crates/testkit`: in-memory stores, a scripted fake model provider, a fake tool, and one contract-test suite per trait.

**Done when:** the in-memory implementations pass every contract suite.

### PR 4 — Postgres stores and dev compose

Durable storage for jobs, events and design state.

- `crates/infra` persistence with `sqlx` migrations: jobs, events, SDR versions, evidence, approvals, artifact metadata.
- `JobStore`: claiming with `FOR UPDATE SKIP LOCKED`, leases, heartbeat renewal.
- `EventLog`: append with gap-free per-run sequence numbers; `LISTEN/NOTIFY` for new events.
- `deploy/compose/` with Postgres for local development.

**Done when:** the testkit contract suites pass against Postgres, two concurrent claimers never get the same job, and a reader resuming after sequence *n* receives *n + 1* onward exactly once.

### PR 5 — SDR v0

The only way to change design state.

- `crates/sdr` exports `SdrManager`; the store is `pub(crate)`.
- Typestate transitions `Proposed → Validated → Committed`; `read(slice, version)`, `propose`, `validate`, `commit`, `reject`.
- Each version records actor, reason, parent version, evidence references and harness-data hashes. Versions are linear.
- `migrations/` started, with the v0 schema as the first version.

**Done when:** property tests show no version is committed without validation and stale base versions are rejected, and a compile-fail test shows an unvalidated transition cannot reach `commit`.

### PR 6 — Checks, gates and approvals

Checks produce evidence; gates decide.

- `crates/evaluation`: checks for schema, units (a unit registry by dimension, with conversion and mismatch errors), and numeric requirement thresholds (decision 6). Each check writes evidence with a verdict.
- `crates/governance`: a tool permission gate from `config/tools/`, an SDR commit gate that requires the configured checks to pass, and an approval gate from `config/policies/autonomy.yaml`.
- Typed decisions `allow`, `deny`, `ask`, `retry`, `reject_transition`; hooks as event subscribers; every decision written to the event log.

**Done when:** a unit mismatch or an unmet threshold rejects the commit with evidence attached, a commit the policy flags returns `ask`, and every decision appears in the event log.

### PR 7 — build123d tool server and dev benchmark

The one tool Increment 1 needs, ported from the legacy build123d tool, and the problem it solves.

- `mcp_servers/build123d/` exposing fixed, typed operations from `legacy/build123d_tool.py`: create a part from parameters, then measure its mass, volume and bounding box. No tool accepts a script to execute.
- Request and result schemas in `schemas/tools/build123d/`; Python models generated from them, with a drift check in CI.
- Dockerfile and a compose service on an internal network with no internet egress and CPU and memory limits. MCP over streamable HTTP inside that network.
- `config/tools/build123d.yaml`: no external side effects, idempotent, timeout, no approval.
- `evals/benchmarks/dev/part_design_001/`: requirements, budgets and checkable expected outcomes, written by the team.

**Done when:** the server's tests pass in its container, it cannot reach the internet, no tool accepts code to execute, and the benchmark files validate against their schemas.

### PR 8 — Tool gateway

The harness side of every tool call.

- `crates/tools`: `rmcp` client over streamable HTTP; a registry from MCP discovery, filtered by the `config/tools/` overlays so unlisted tools are unavailable.
- The execution pipeline from blueprint §14.2: validate, gates, execute with timeout and cancellation, normalize, evidence.
- Step records with idempotency keys, so recovery returns a completed call's stored result instead of calling again.

**Done when:** against the build123d container, a valid call returns a normalized result with evidence, an unlisted tool is denied, a timeout returns an error observation rather than success, and a replayed step returns the stored result.

### PR 9 — Model gateway

Two providers behind a provider-neutral contract (ADR-0027).

- `crates/model-gateway`: two adapters, `anthropic` (Claude, Messages API) and `openai_compatible` (the internal Pliny platform). Each normalizes messages, tool-call requests, tool results, stop reasons and usage; provider types stay inside the adapter. Claude's thinking blocks travel as the new `ContentBlock::Opaque` and go back unchanged.
- `config/models/routing.yaml`, checked by `cargo xtask validate-config`:
  - Three Claude profiles on the one `anthropic` provider: `haiku` (the default), `sonnet` and `opus`. `SYNTH_MODEL_PROFILE` overrides the default. An agent may name its own profile.
  - Each profile declares `tool_calling`. `pliny` has `false`, and startup refuses it for an agent with tools.
  - Keys and the Pliny base URL come from environment variables. A missing one fails at startup, naming it.
  - TLS verification stays on, with an optional internal CA bundle.
- Bounded retries for rate limits, overload and timeouts. Every request has a timeout and honours cancellation.
- Token and cost accounting per call, with prices from config.
- A recorded-response provider in testkit, so CI never calls a live model.

**Done when:** both adapters pass the `ModelProvider` contract suite against recorded responses, including a tool-call round trip for the Anthropic adapter, and startup refuses the Pliny profile (`tool_calling: false`) for an agent with tools.

### PR 9.1 — Pluggable model providers

Added after PR 9; not on the critical path (ADR-0028). Each provider becomes one self-contained unit, so it can be removed or added without touching the rest of the gateway.

- Profiles keep the fields every provider shares, plus `settings` that follow the provider's own schema in `schemas/config/model-providers/`. `routing.yaml` moves to schema version 0.2, with the same profiles.
- `ProviderFactory` and `ProviderRegistry` in `crates/model-gateway`. `providers::default_registry()` is the one list of registered providers. An unregistered provider, or invalid settings, fails at startup and in `validate-config`.
- One module per provider holds its adapter, factory, tests, recordings and live smoke test. The gateway's own tests run over fake providers.

**Done when:** every PR 9 test still passes, and the live Claude round trip still passes. Removing either provider by the four-step checklist in ADR-0028 leaves `cargo build`, the gateway and schema tests, `validate-config` and clippy passing, with no other edit.

### PR 9.2 — Tool calling on Pliny

Not scheduled; not on the critical path. It starts once Pliny returns tool calls. The full plan, with its entry gate and stop conditions, is in [pliny-tool-calling.md](pliny-tool-calling.md).

- Capture a real Pliny tool round trip as a scrubbed recording, and replay it in CI.
- Set `tool_calling: true` on the `pliny` profile, and update the tests that pin the refusal.
- Add an ignored live test, `live_pliny_tool_round_trip`.

**Done when:** the live Pliny tool round trip passes, the captured one replays in CI, and agents with tools may use `pliny`, while profiles with `tool_calling: false` are still refused.

### PR 10 — Context v0

What each model call sees, assembled rather than accumulated.

- `crates/context`: a builder from the task contract, an SDR slice and recent relevant events, with a token budget per section.
- Deterministic assembly; the context hash is written to the event log.
- Compaction rebuilds from the SDR and job state instead of summarizing the conversation.
- Trusted instructions and untrusted tool output in separately labelled sections.
- `prompts/system/` and `prompts/orchestrator/` v0, with headers.

**Done when:** golden tests pin the assembled context for fixed inputs, the same inputs give the same hash, and an over-budget run rebuilds within budget without losing SDR facts.

Settled while building, and recorded in [ADR-0029](../adr/0029-context-assembly-v0.md):
- Sizes are estimated (`ceil(bytes / bytes_per_token)`, no tokenizer). Budgets are named profiles in `config/context.yaml` under a ceiling in `config/policies/context.yaml`.
- `context_assembled` gains `sdr_version` and `last_event_seq`, and `ErrorCode` gains `context_overflow`.
- The builder appends `context_assembled` itself; PR 11 must not.
- `config/agents/main_orchestrator.yaml` (PR 11) names `prompt: orchestrator/main.md`.

Follow-ups, not in this PR:
- Model-gateway adapters map a provider's "prompt too long" to `context_overflow`.
- A `context_window` per model in the routing schema, and a limit of `min(ceiling, window − max output − headroom)`.
- Blueprint §9.1 (profiles may change within the ceiling) and §24 edits.
- An optional `context_profile` in agent specs once a second agent exists.
- `validate-config` checks that each agent's `prompt` names a loaded prompt, once `config/agents/` exists.

### PR 11 — Query loop and orchestrator

One orchestrator that plans, calls tools and proposes design changes, with gates deciding what sticks.

- `crates/runtime`: a query loop that returns a `Stream` of typed events.
- The orchestrator is built from `config/agents/main_orchestrator.yaml` through the agent registry, using the `bounded_worker` loop type. No subagents.
- The model reads and changes the design only through `sdr_read` and `sdr_propose` tools; proposals commit only through the gates.
- Budgets for tokens, tool calls and wall time; bounded repair attempts; the stop reasons from blueprint §5.4.

**Done when:** with the fake model and fake tool, scripted runs reach every stop reason, a model claiming success without passing checks does not stop with `success`, and an exhausted budget ends the run with `budget_exhausted`.

Settled while building, and recorded in [ADR-0030](../adr/0030-query-loop-v0.md):
- `sdr_read` and `sdr_propose` run in process through `SdrManager`, not as MCP tools or through the tool gateway. Their schemas are in `schemas/tools/sdr/`. The harness sets a proposal's id, base version, actor and schema version.
- Every tool result follows `schemas/tools/tool-result.schema.json`.
- `governance::WriteScopeGate` (`sdr_write_scope`, `deny`) enforces the task's write scope before commit.
- A turn closes on a reply without a tool call or on a commit. The calls after a committing proposal in the same reply are not run.
- A failed or denied call, an invalid or rejected proposal, and a reply without success each count one repair. The run fails on the first failure beyond `max_repair_attempts`. `main_orchestrator` uses `on_repair_exhausted: fail`, and the registry refuses `escalate` until Increment 3.
- Each turn that closes sets `job.pending_work` to what is unmet.
- A run succeeds only right after a final commit through every gate, with every must-level and task-named requirement passing on that commit's checks. The §5.4 artifact clause is met trivially until artifacts exist.
- With no tool call left, the run stops before its next model call.
- `approval_required` returns a `Continuation`; `Orchestrator::resume` continues it in memory.
- `model_call_completed` has no cost field.
- The ignored live test showed that the API accepts a context rebuilt mid-turn, so the context is not pinned within a turn.
- `config/runtime.yaml` holds the loop's I/O timeout.
- `futures-core` is a new direct dependency, for the `Stream` trait only.

Follow-ups, not in this PR:
- PR 12 persists the continuation (open turn with its thinking blocks, pending call, counters) and the job, with a schema.
- PR 12 detects a transition that committed before a crash, by its deterministic id.
- PR 12 accounts for the `tool_call_completed` a replay does not write (ADR-0026).
- PR 12 adds job states, `job_state_changed`, waking on an answered approval or a cancel, and bootstrap.
- A provenance check in `evaluation`: a parameter's value and source must match the evidence it cites. Until then the benchmark's `parameter_matches_reference` outcome catches a fabricated value (PR 15).
- PR 13 decides where the result's narrative `summary` comes from.

### PR 12a — Lease fencing

A worker whose lease was reclaimed cannot write. Lands before PR 12, which makes leases expire and jobs move between workers.

- Every store write takes a `WriteAuthority`: the `FenceToken` from the job's claim, or one of four `UnfencedReason`s (`JobInsert`, `JobWake`, `ApiCommand`, `ProjectCreate`), an allow-list the store enforces, failing closed.
- Each claim raises `jobs.lease_epoch` (migration `0003_lease_fencing.sql`). Each store checks the fence in the write's own transaction, on the database clock, before any other condition: a share lock on the job row that a reclaim cannot pass until the write commits.
- A stale fence fails with the new `ErrorCode::LeaseLost`, not retryable. The worker abandons the run and never marks the job failed.
- `ExecutionContext` gets a builder and carries the authority; children inherit it. Every writer passes `ctx.write_authority()`.
- `Tool::invoke` is reached only through the fenced `tool_call_started` append.
- `cargo xtask arch-check` limits who issues fences, grants authority, names `UnfencedReason` and invokes tools.

**Done when:** every store rejects a write with a stale epoch and accepts one with the current epoch (one fencing suite, in memory and on Postgres); a worker whose job was reclaimed cannot append an event, record a step or commit; and every existing contract suite still passes against both backends.

Settled while building, and recorded in [ADR-0032](../adr/0032-lease-fencing.md):
- Other crates' tests get authority only from `testkit::leases`: `LeasedJob`, `fenced_ctx`, and one helper per unfenced reason, so they never name `UnfencedReason`.
- `crates/api` does not get `UnfencedReason`: in PR 14, bootstrap hands it a pre-built `ApiCommand` authority.
- `FakeTool::before_answer` is the pause point for crash and race tests, with `Slot` for what the hook needs from its world.

Follow-ups for PR 12, listed in ADR-0032:
- The scheduler builds the fenced root context; the heartbeat, checkpoints and recovery write under the fence.
- ~~The waker's `job_state_changed {suspended → queued}` needs a new unfenced reason.~~ Corrected in PR 12: no new reason. Each reason covers one operation across stores, so `JobWake` permits the wake and its `job_state_changed` (ADR-0032 amendment). ADR-0031 records that `crates/api` stays outside the `UnfencedReason` rule.
- PR 12's migrations are `0004_job_status.sql` and `0005_scheduler.sql`.

### PR 12 — Scheduler, worker and recovery

Runs that outlive processes.

- `runtime/scheduler`: job states, claiming, leases and heartbeats, and waking on events (an answered approval, a cancel).
- Recovery rebuilds a run from the job store, the SDR version and the event log, never from memory.
- `crates/bootstrap` as the composition root, and the `apps/worker` binary.

**Done when:** a system test kills the worker mid-run and restarts it, and the run completes with no repeated tool call or commit and a gap-free event sequence. A suspended run survives a restart and resumes when its approval is answered.

Settled while building, and recorded in [ADR-0031](../adr/0031-scheduler-and-recovery-v0.md) and the PR 12 amendments of [ADR-0032](../adr/0032-lease-fencing.md):
- **Job status.** It is one tagged type, `status: JobStatus` (job schema 0.2): queued, running, waiting, suspended, completed, cancelled or failed, each with the fields it needs. typify 0.8 cannot generate the schema `if`/`then` the rule was first written as. `job_state_changed.to` is a `JobStatus`, and Postgres keeps an indexed `state` column generated from the document.
- **Write-ahead transitions.** A transition out of running is checkpoint, event, save, since the save ends the fence. A claim finishes a decided transition only if the job holds the checkpoint the event names.
- **Events reference durable data only.** They carry versions and hashes; stores carry payloads. No opaque or untrusted data goes into the log.
- **The continuation record** (`schemas/jobs/continuation.schema.json`) is saved at every phase boundary under the fence, numbered, and referenced by its canonical sha256. Only the latest is kept.
- **Recovery.** It reads the job, its record and the log on every claim. A `run_stopped` after the log's last state change is finished without running again (found by the crash point `stopped_before_transition`).
- **Unfenced reasons.** `JobInsert` is renamed `JobSubmit`. Each reason covers one operation across stores, so the waker needs no new reason. Unfenced submit is for top-level runs only.
- **Waking.** No `mark_checked`: the park save writes `wake_checked_seq`, and scans read only the wake-relevant kinds through `EventLog::read_kinds`.
- **Unique completion events.** `run_started`, `tool_call_completed`, `sdr_transition` and `approval_requested` happen once per key. A lost one is repaired with `recovered: true`.
- **Idempotency.** Keys are `{job_id}:{step_id}`, sent to tools in MCP `_meta`. An interrupted non-idempotent call is `outcome_unknown`. A tool is `idempotent: true` only with a named `idempotency_test`; build123d is `idempotent: false`.
- **Approval ids** derive from the job, the subject and its content hash, so a re-run step raises the same approval.
- **Environments.** `SYNTH_ENVIRONMENT` (unset is Production) is passed to every config section's validation. A model `base_url` may be loopback http only in Test.
- **Bootstrap** builds the worker after its startup checks, installs ring as the process TLS provider, and refuses a registered check that `gates.yaml` does not require. `apps/worker` releases its job on a signal; its exit codes are 0, 2 and 1.
- **Tests.** The system tests are the `system` test of `synth-bootstrap`, at `tests/system/`. The process-kill test is an ignored test there, not a test of `synth-worker`: arch-check counts dev-dependencies, and the shared test support names crates an app may not depend on. `cargo xtask test-tools` builds the binary and runs the test. arch-check scans `tests/`.

Follow-ups, not in this PR:
- The replay store of content-addressed context sections, before baselines (ADR-0029, ADR-0031).
- A repeat-with-the-same-key test for each build123d tool, then `idempotent: true`.
- An optional NOTIFY nudge inside the Postgres adapter, if polling proves slow.
- An ADR for `config/environments/`, when a second environment needs its own values.
- The wake condition for a parent waiting on a child, and the kernel crash point (Increment 3).
- Untrusted text in `error` fields of the log (ADR-0032 amendment).
- From ADR-0029 and ADR-0030: the provider overflow mapping, per-model context windows, the provenance check.

### PR 13 — Run report and artifacts

The record of what a run produced and why.

- `crates/artifacts`: artifact metadata (format, source SDR version, producing tool and version).
- Run report assembly from the SDR, evidence, job record and harness-data hashes, validated against `schemas/reports/`. Only the narrative field is model-written.

**Done when:** the report for a fixture run validates, every requirement status links to evidence, and every number in it matches the evidence store.

### PR 14 — API v0 and api-server

The external contract, shared later with the frontend.

- `schemas/api/openapi.yaml` v0: create a run, get a run, stream its events (server-sent events with `Last-Event-ID`), answer an approval, cancel, get the report, get an artifact.
- `crates/api` and `apps/api-server`; development-token authentication (decision 8).
- Contract tests that validate every response against the OpenAPI document.

**Done when:** the contract tests pass, a client that disconnects mid-run and reconnects with its last ID receives every later event exactly once, and requests without the token are rejected.

### PR 15 — CLI, end-to-end run and baseline

The exit criterion, proved through the CLI.

- `apps/cli` (`synth`): `run start`, `run watch`, `run approve`, `run cancel`, `run report`. Payloads use `schema-types`; the CLI holds no logic.
- An end-to-end test in `tests/system/` through the CLI, with the real build123d container and the recorded model.
- `cargo xtask eval` for repeated live runs, with results written to `evals/baselines/`.
- A developer guide for running the stack locally.

**Done when:** all six exit criteria in the scope section pass and the baseline is recorded.

## Out of scope

These are deliberately left out of Increment 1, so no crate or folder for them is created yet.

| Item | Lands in |
| --- | --- |
| Frontend and BFF integration | After the backend architecture matures |
| SDR branches, invalidation, backtracking, `process` levels | Increment 2 |
| Agent-written CAD scripts, a per-task code-execution sandbox, `cad_views` | Increment 2 |
| Subagents and delegation | Increment 3 |
| Formalized requirements with a solver (NL-to-FOL) | Increment 3 |
| `knowledge` crate and the knowledge-graph server | When a benchmark problem needs reference data |
| `improvement` crate and the held-out split | Increment 4 |
| Asynchronous tool jobs and timer wake-ups | With the first long-running tool |
| OpenShell backend, persistent CAD kernel | Open Decisions 3 and 4 |
| Real authentication | Open Decision 9, with the frontend |
| Docker images for `api-server` and `worker`; Kubernetes | When a shared environment exists |

## Risks

The biggest risk is SDR v0 churning while the design workflow is still unsettled.

| Risk | Mitigation |
| --- | --- |
| SDR v0 schema changes as the design workflow is decided | Keep v0 minimal (decision 5); migrations from PR 5; settle the design workflow before Increment 2 starts. |
| typify cannot map some schema constructs | Use a plain JSON Schema subset; PR 2's example instances exercise every construct used. |
| The legacy build123d tool runs arbitrary scripts, or its image is heavy | PR 7 wraps only fixed, typed operations and starts right after PR 2 on its own track, off the critical path. |
| Live model runs vary, so tests flake | CI uses recorded responses only. Live runs happen in `cargo xtask eval`, repeated, with spread reported. |
| PRs 11 and 12 are large and sequential | Split each into reviewable commits; land the job-store parts of the scheduler with PR 4. |
| A worker that lost its lease keeps writing | PR 12a: every write carries an authority checked in its own transaction. |
| Tests couple to Postgres | Every store has a trait and a testkit contract suite that runs against both in-memory and Postgres implementations. |

## Exit checklist

Increment 1 closes when every item below is ticked.

- [ ] The six exit criteria in the scope section pass: in CI with recorded responses, and in `cargo xtask eval` with the live model.
- [ ] The proposed ADRs (0015, 0016, 0020, 0021, 0022, 0027) are accepted, and decisions 3, 4 and 7 are recorded in the benchmark and config.
- [ ] Baseline results are committed to `evals/baselines/`.
- [ ] A new team member can run the benchmark from a clean checkout using the developer guide.
- [ ] Questions raised during the increment are logged for Increment 2 planning.

**Blueprint §24 needs a matching edit:** Increment 1's exit runs through the CLI instead of the UI, the frontend and BFF move to a later increment, and the numeric requirement check (decision 6) is added.
