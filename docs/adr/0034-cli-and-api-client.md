# ADR-0034: The CLI, the API client and benchmark run requests

- Status: Proposed
- Date: 2026-10-26

## Context

Increment 1 exits through a CLI (`synth`) that is a thin client of api-server (increment plan, PR 15). Blueprint §10 lists `apps/cli` but no client library, and ADR-0018 left turning a benchmark problem into a `RunRequest` to PR 15, outside the CLI, which holds no logic. Before PR 15a, these were undecided:

- where the typed client of API v0 lives: HTTP calls, server-sent event parsing and resuming with `Last-Event-ID`. The CLI, the system tests and `cargo xtask eval` (PR 15b) all need it, and `apps/*` may depend only on `bootstrap` and `api`;
- how a benchmark problem becomes a `RunRequest`, and which crate does it;
- how `synth run watch`, run again after an interrupt, resumes from the last event it received (exit criterion 3).

## Decision

**A client crate, `crates/api-client`** (package `synth-api-client`, library `api_client`), may depend on `contracts` only.

- `apps/*` may depend on `bootstrap`, `api` and `api-client`. `bootstrap` may depend on it like every other crate.
- It holds:
  - `Client`, one method per API v0 operation;
  - `events`, a stream that reconnects with `Last-Event-ID` after a transport error, a bounded number of times;
  - `ClientError`;
  - `IdempotencyKey`;
  - `EventJournal`.
- Request and response bodies are the generated types in `contracts::types`. The client builds no task and decides nothing; the server does.
- Every request has a time limit. The token is never logged, and its `Debug` is redacted.
- TLS goes through rustls with the ring provider chosen in code, and the platform verifier, as in the model gateway (ADR-0027). A build that unifies reqwest's features with the gateway's otherwise has no crypto provider, and building a client panics. Redirects are never followed, so the token goes only to the server named.

**A benchmark problem becomes a `RunRequest` in `evaluation::benchmarks::run_request`.** It is a pure function of the problem and the agent's spec:

| `RunRequest` field | From |
| --- | --- |
| `agent_id` | the agent spec's `id` |
| `name` | the problem's `id` |
| `description` | the problem's `description`, then each given's `text` under "Givens" |
| `goal` | the agent spec's `goal` |
| `acceptance_criteria` | one per requirement: its id, text and quantity |
| `requirements` | the problem's requirements, as SDR requirements without evidence |
| `budget` | the agent's limits, narrowed by the problem's tool calls, wall time (in seconds, through `evaluation::units`) and repair attempts |

- `notes`, `expected_outcomes`, `feasibility` and `status` are never copied. A task never sees its grader or its hints (ADR-0025).
- The function lives in `evaluation`, not `infra`, so the improvement loop (`improvement` may depend on `contracts`, `runtime` and `evaluation`) can build the same requests. `infra::benchmarks` keeps reading the files.
- `cargo xtask benchmark-request <id>` loads the problem and the agent spec, calls it, checks the result against `schemas/api/run-request.schema.json` and prints it. `synth run start <file>` sends any `RunRequest`, from a benchmark or not.

**`synth run watch` keeps a journal.** Each event is appended as one JSON line to `<state-dir>/runs/<run_id>/events.jsonl`, in a single write, before it is printed.

- On start, the CLI drops a torn last line and resumes after the last complete one, with `Last-Event-ID`. The journal is the cursor, so an interrupt at any point, a hard kill included, leaves it without gaps or duplicates.
- `--state-dir` defaults to `runtime-data/cli`, which is never committed.

**The CLI surface.**

| Command | API |
| --- | --- |
| `synth run start <request.json> [--idempotency-key <key>]` | `POST /v0/runs` |
| `synth run watch <run_id> [--state-dir <dir>]` | `GET /v0/runs/{run_id}/events` |
| `synth run approve <run_id> <approval_id> --answer <approve\|reject> [--comment <text>]` | `POST /v0/runs/{run_id}/approvals/{approval_id}` |
| `synth run cancel <run_id> [--reason <text>]` | `POST /v0/runs/{run_id}/cancel` |
| `synth run report <run_id>` | `GET /v0/runs/{run_id}/report` |

- The server is `SYNTH_API_URL`, `http://127.0.0.1:8080` by default (`config/api.yaml`). The token is `SYNTH_API_TOKEN`, never a flag, so it stays out of shell history.
- Output is the API's JSON body; `watch` prints one event per line.
- Exit codes are those of the other binaries: 0 on success, 2 for a usage error, 1 for anything else. An API error prints its `ApiError` body, which includes the correlation id, to stderr.
- Without `--idempotency-key`, `start` uses a fresh key: a sha256 of the request, the time and the process id. The key is printed to stderr so a retry can reuse it.

## Alternatives considered

- **The client inside `apps/cli`:** `cargo xtask eval` would copy the HTTP code or run the binary and parse its output.
- **A `client` feature of `crates/api`:** no new crate, but every server module behind a feature gate, and one package for two sides of the wire.
- **`synth run start --benchmark <id>`:** one step, but the client would link the harness through `bootstrap` to build the request.
- **A benchmark route on the server:** ADR-0018 already rejected a benchmark id as the request.
- **The mapping in `infra::benchmarks`:** next to the loader, but out of reach of the improvement loop.
- **A cursor file written after printing:** a kill between the two repeats an event; written before, a kill loses one.
- **clap for the command line:** a new dependency for five commands; the other binaries parse by hand.

## Consequences

- The CLI and xtask share one client, and the frontend later generates its own from `openapi.yaml`. Contract changes still start in `schemas/api/`.
- Givens reach the model as text in the brief. Typed givens (for example SDR version 0 parameters with source `user`) would be an additive `RunRequest` 0.2.
- `max_tokens` and `max_steps` still come from the agent spec, since the benchmark format has neither (ADR-0025).
- The end-to-end test (`tests/system/cli_e2e.rs`) runs the `synth`, `api-server` and `worker` binaries. Like `worker_process.rs`, it is ignored by default and run by `cargo xtask test-tools`.

## Links

- Blueprint §3.2, §3.5, §10, §10.3, §13.1, §17
- ADR-0018, ADR-0022, ADR-0025, ADR-0031
- Increment 1 plan, PR 15a; `crates/api-client`, `apps/cli`, `crates/evaluation/src/benchmarks.rs`, `xtask/src/main.rs`
