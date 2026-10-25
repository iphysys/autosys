# ADR-0018: Schema-first external API v0

- Status: Proposed
- Date: 2026-10-26

## Context

Blueprint §3.5 sets the API's rules: an OpenAPI 3.1 document in `schemas/api/` that references the JSON Schemas, handlers checked against it by contract tests, a run event stream as server-sent events that resumes from a sequence number, and commands that are requests, never state edits. The blueprint lists this ADR as Proposed but no record existed. PR 14 builds the first version, the one the CLI (PR 15) and later the frontend use. Before PR 14, these were undecided:

- what a client sends to start a run, and how the run's requirements reach the SDR, whose version 0 was always the empty design and changed only by a fenced commit;
- where the API's contract tests run, since `api` may depend only on `contracts` and `runtime` (§10.3), and the real submit and report paths need an `SdrManager`;
- which HTTP stack serves it.

## Decision

**A run starts from a brief and its requirements.** `POST /v0/runs` takes a `RunRequest`: an agent, a name, description, goal and acceptance criteria, the requirements, and an optional budget.

- The server builds the task. Tools, skills and result schema are the agent's own. The read slice covers every kind; the write scope covers parameters, decisions and assumptions, because requirements are the client's. The budget is the agent's limits unless the request narrows them. `AgentRegistry::agent_for` checks the task, so a task the worker would refuse is refused at submit time.
- Each run gets a new project. **Its SDR version 0 holds the requirements**, with an empty design. `SdrManager::create_project` applies them as `add_requirement` operations and checks the document schema; requirements that cite evidence are refused. Version 0 is still one write, under the existing `ProjectCreate` reason, so no new unfenced reason is needed. Its record names the caller as actor and the server's build.
- The client sends an `Idempotency-Key`. The run, job, project and task ids are a sha256 of it under their prefixes, so the same key and request return the same run (200). A submit that stopped partway is finished by the next one, and the same key for a different request is 409. Only a submit whose insert succeeds records `job_state_changed {→ queued}`; if a crash loses that record, the claim adds it (ADR-0031).
- Turning a benchmark problem into a `RunRequest` is left to PR 15.

**The API reaches the harness through contracts.** Two traits are added to `contracts::runs`. `RunSubmitter` is implemented by `runtime::Submissions`, which lives in the scheduler module, the only runtime code that names an unfenced reason. `RunReports` is implemented by `runtime::Reports`, over `artifacts::ReportAssembler`. testkit fakes both and runs a contract suite against fakes and real implementations alike. `api` depends on `contracts` alone.

- Bootstrap hands `api` the `ApiCommand` authority and the development actor `{kind: user, user_id: dev}` (ADR-0022). `api` never names `UnfencedReason` (ADR-0031, ADR-0032).
- An answer and a cancel are events appended with that authority. An answer needs an `approval_requested` for that id in the run and no earlier answer (otherwise 404 and 409). Neither command is accepted once a run is completed, cancelled or failed (409). Two answers sent at once can both be stored; the scheduler already uses the earliest.

**The routes**, under `/v0`:

| Method and path | Success | Errors |
| --- | --- | --- |
| `POST /runs` | 201 or 200 `Run` | 400, 401, 409, 413, 415 |
| `GET /runs/{run_id}` | 200 `Run` | 400, 401, 404 |
| `GET /runs/{run_id}/events` | 200 `text/event-stream` | 400, 401, 404 |
| `POST /runs/{run_id}/approvals/{approval_id}` | 202 the stored event | 400, 401, 404, 409, 413, 415 |
| `POST /runs/{run_id}/cancel` | 202 the stored event | 400, 401, 404, 409, 413, 415 |
| `GET /runs/{run_id}/report` | 200 `RunReport` | 400, 401, 404, 409 until the run stops |
| `GET /artifacts/{artifact_id}` | 200 `ArtifactMetadata` | 400, 401, 404 |
| `GET /artifacts/{artifact_id}/content` | 200, the metadata's media type, `nosniff` | 400, 401, 404 |

- `Run` is the client's view of a run: its id, project, `JobStatus` (which names a pending approval), times and `last_seq`. The job record is not exposed.
- Every error body is an `ApiError`, which is the common `ErrorInfo`. InvalidInput maps to 400, NotFound to 404 and Conflict to 409. Unavailable, Timeout and Cancelled map to 503, and anything else to 500. A 5xx body says nothing about its cause; the cause is logged under the request's correlation id, which every response carries in `X-Correlation-Id`.
- Request bodies must be `application/json` (else 415) and fit the body limit (else 413). They are checked against their schema with every problem listed (400). Requests other than event streams have a time limit.

**Event stream.** Each frame's `id` is the event's sequence number, `event` its payload type and `data` the event. `Last-Event-ID` resumes after that number. The log numbers a run's events without gaps and the stream reads it in order, so a reconnecting client receives every later event exactly once. A worker records an event before the save that leaves running, so the stream ends after the job is seen completed, cancelled or failed and one more read finds nothing. A suspended run's stream stays open, with keep-alive comments, until the server shuts down.

**Authentication** is ADR-0022's token, `SYNTH_API_TOKEN`, sent as a bearer token and compared in constant time (`subtle`). Every route, unknown paths included, needs it (401 with `WWW-Authenticate: Bearer`). Without it, or with it empty, api-server does not start.

**Stack and settings.** axum 0.8, on the hyper 1 and tower 0.5 already in the tree. `config/api.yaml` (`schemas/config/api.schema.json`) sets:
- the loopback listen address;
- the request time limit and body limit;
- the stream keep-alive and page size;
- the pool;
- `artifact_root`, the artifact volume (ADR-0016), `runtime-data/artifacts` by default (blueprint §10).

Both binaries migrate the database at startup.

**Contract tests** live in `crates/api/tests/contract.rs` and run in memory on testkit's stores and fakes. Every documented (operation, status) pair is produced, and its status, media type and body (each frame's data, for the stream) are checked against `openapi.yaml`. A documented pair never produced fails the test. `tests/system/api.rs` repeats the done-when cases on Postgres through a real server and worker.

## Alternatives considered

- **A benchmark problem id as the request:** matches PR 15's run, but the task budget the format lacks would have to be invented (ADR-0025), and a client with its own requirements could not start a run.
- **The full `Task` from the client:** the client would need to know tools, slices and result schemas, and the CLI would build tasks, which is harness logic in a client (§3.2).
- **Requirements as version 1, under a new unfenced reason:** a second write per submit and a wider allow-list, for no gain over a version 0 that holds them.
- **Contract tests only on Postgres, in `tests/system`:** no new traits, but they leave the place §16.1 gives them and need a database for every check.
- **hyper alone:** routing, extractors, SSE framing and graceful shutdown written by hand.
- **Random ids from the server:** a retried create would start a second run.

## Consequences

- The CLI and the frontend share one generated contract. TypeScript generation from `openapi.yaml` arrives with the frontend.
- A project holds exactly one run in Increment 1. Several runs on one project wait for SDR branches (Increment 2).
- Authorization per project is Open Decision 9; until then the one token can do everything.
- ADR-0033's "the API reaches the assembler through `runtime`" holds through `RunReports`: runtime implements it, and `api` names only the trait.

## Links

- Blueprint §3.2, §3.5, §10.3, §13.1, §16.1, §17
- ADR-0016, ADR-0020, ADR-0022, ADR-0025, ADR-0031, ADR-0032, ADR-0033
- Increment 1 plan, PR 14; `schemas/api/`, `schemas/config/api.schema.json`, `crates/api`, `crates/contracts/src/runs.rs`, `crates/runtime/src/scheduler/submit.rs`, `crates/runtime/src/reports.rs`, `apps/api-server`
