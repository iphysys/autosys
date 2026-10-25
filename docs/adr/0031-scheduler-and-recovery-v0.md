# ADR-0031: Scheduler and recovery v0

- Status: Proposed
- Date: 2026-10-25

## Context

PR 11 ran a job's run in memory. A worker crash lost the open turn, an approval survived only inside its process, and nothing gave a job a state, a lease or a worker. Blueprint §6 asks for runs that outlive processes: a worker claims a job, keeps it leased by heartbeat, and after any crash a new worker rebuilds the run from the job store, the SDR and the event log (§6.3), with no tool side effect or commit repeated. ADR-0032 (lease fencing, PR 12a) makes a worker whose lease was reclaimed unable to write; this ADR builds the scheduler on it.

The blueprint leaves open: which states a job has and what each carries; how a state change is recorded when the save that makes it also ends the writer's fence; what a run persists, and when; how a claim decides what to do next; what counts as a failed attempt; how a tool call that was interrupted is told apart from one that never ran; how parked jobs wake; and how a worker process is built and stopped.

## Decision

### Job states

A job's status is one tagged type, `JobStatus` (job schema 0.2, see ADR-0032 "a job's status is one tagged type"):

| From → to | Made by | Authority | Recorded |
| --- | --- | --- | --- |
| none → queued | `Scheduler::submit` | `JobSubmit` | insert, then `job_state_changed` |
| queued → running | a claim | the new fence | claim, then the record |
| running → running | a claim of an expired lease | the new fence | claim, then the record, reason `reclaimed` |
| running → suspended | the worker, on `approval_required` | fence | checkpoint, event, save |
| suspended → queued | the waker | `JobWake` | wake, then the record |
| running → queued | the worker: a retryable error, or release on shutdown | fence | checkpoint, event, save |
| running → completed, cancelled, failed | the worker | fence | checkpoint, event, save |

`waiting` is reserved for timers, tool handles and child jobs; nothing enters it yet.

**Write ahead when the fence ends.** A save that leaves running clears the lease, and with it the fence, so nothing can be appended under that fence afterwards. A fenced transition therefore writes ahead:

1. a fenced checkpoint save, the job still running;
2. `job_state_changed {running → X}` under the fence, naming that checkpoint by number and sha256;
3. the save into X.

The save writes nothing the event and the checkpoint lack. Unfenced transitions (submit, wake) are conditional store operations that can lose a race, so they act first and record after.

**The claim's reconcile.** A claim reads the run's last `job_state_changed`. If the claim is interrupted, that record is `{running → X}` with X not running, and the row has not seen it, a dead worker decided a transition and did not save it: recovery step 0 finishes it. Otherwise the claim appends, under its fence, whatever records are missing on the way from the log's last state to its own: the submit's, the wake-up's, an earlier claim's. Then it appends its own record. Each `from` equals the previous `to`, and the chain ends in the job's state.

### The continuation record

What a run needs to continue is a persistence record, `ContinuationRecord` (`schemas/jobs/continuation.schema.json`), kept apart from `Job` (agent execution state versus job state, §3.4) in `jobs.continuation`, written by the fenced `save` in the same statement as the job.

- **Fields:** `checkpoint` (a number, +1 per save while running), the run id, repair failures, models used, committed transitions, evidence ids, the head and last commit (by transition id, version and evidence ids, never their content), the open turn's messages, and the open step: number, SDR version, prompts, the assistant's reply, the results so far and the next call. An `approval_id` requires an open step.
- **Opaque blocks** (for example thinking signatures) are kept unchanged (ADR-0027). `crates/runtime/src/continuation.rs` converts between the record and the loop's state losslessly.
- **Its reference** is `{version, sha256}`, the sha256 of the record's canonical JSON (`contracts::canonical_sha256`, keys sorted at every depth), so a record read back from jsonb hashes the same.
- **When it is written:**
  - C1: after a model reply is forwarded.
  - C2: after each tool result, when a turn closes, and when a commit lands.
  - C3: on `approval_required`, with the approval id, before `{running → suspended}`.
  - C4: at a terminal stop, with the final counters, before the transition.
  - Before a requeue or a release.
  - Never before a model call. The record is kept at the end, so the run report and a waiting parent read the result from it.
- **Only the latest record is kept.** Exact replay of a run (ADR-0029) will come from content-addressed context sections in a replay store, a follow-up before baselines, not from a history of checkpoints.

### Recovery

On every claim, after the reconcile:

0. **A decided transition without its save:** if the row holds the checkpoint the event names, save X under the new fence with `progress.last_event_seq` set to the event's seq. Nothing runs and nothing is appended. A queued X gets its retry delay from the stored `failed_attempts`. Any other record is a broken invariant: the job fails with `harness_error`.
1. **A `run_stopped` after the log's last state change:** the previous holder's run stopped and nothing recorded it. Its transition is made from the stored record, without running. The final checkpoint is written after the stop, so the job's own progress mark may already be past it, and the log's last state change is the bound.
2. **A `cancel_requested` after the last `run_stopped`:** the run starts with a cancelled context and records `run_stopped(cancelled)` without a model call.
3. **A record with an approval id:** the run continues with the earliest answer to it; without one, the job suspends again on the same record.
4. **Otherwise:** the run continues from the record, or from the start without one. `run_started` is written only if missing, and a `model_call_completed` already in the log counts its tokens and step.

What each crash point leaves and what recovery does is the table in `tests/system/crash_points.rs`, which runs every point on Postgres.

### Nothing done twice

- **Tool calls.** The idempotency key is `{job_id}:{step_id}`, hashed if longer than an identifier (amends ADR-0030's `{run_id}:…`, the same while one job serves one run). It is in the step record before the call and reaches the tool as `_meta["autosys/idempotency_key"]`.
  - A tool may declare `idempotent: true` only with an `idempotency_test: <file>::<test>` that repeats a call with one key; `cargo xtask validate-config` checks that the file exists. build123d is `idempotent: false` until it has one.
  - An interrupted call of a non-idempotent tool is not repeated: it is `ErrorCode::OutcomeUnknown` (`outcome_unknown`). The model sees it as an error result and may repair once.
- **Commits.** The transition id is deterministic, so `SdrManager::find_commit` finds a commit that landed before the crash at base + 1 (versions are linear, ADR-0020).
- **Lost completion events.** Each component repairs its own: the tool gateway rebuilds `tool_call_completed` from the step record, and the runtime rebuilds `sdr_transition` from the version, both with `recovered: true` and the builder of the normal path.
- **Unique completion events.** `run_started` per run, `tool_call_completed` per step id, `sdr_transition` per transition id and `approval_requested` per approval id. An append of one already there returns the stored event without using a sequence number (Postgres: `dedupe_key` under a unique index, checked under the run's counter lock).
- **Approvals.** The approval id is `appr-` and 32 hex digits of sha256 over the job, the subject (a tool call's step id, or a transition id, which derives from its step) and the subject's canonical hash. A step run again after a crash raises the same approval; a new proposal gets a new one. The gate is not in the hash: one decision asks for at most one approval.

### Claims, heartbeats and retries

- **One job per worker process at a time.** Leases and `not_before` use database time only (ADR-0023).
- **Heartbeat.** The heartbeat renews the lease every `heartbeat_ms`. It is started through an injected `contracts::Spawner`, whose handle aborts the task on drop; bootstrap gives a tokio spawner with a task tracker, testkit a manual one. On `lease_lost`, or when no renewal has succeeded for lease − heartbeat, it cancels the run's lease token. The worker then drops the run like a crash: nothing more is written (ADR-0032).
- **A failed attempt** is a retryable error (the error's own flag) or an interrupted claim; a lost lease is never one for the losing worker, and finishing a decided transition (step 0) is not one either.
  - Each failed attempt is kept in `attempt_history` with its error and progress mark. `failed_attempts` resets when a checkpoint moves the progress mark past the claim's.
  - A requeue waits `min(backoff_min × 2^(n−1), backoff_max)`, computed from the stored count, so a new owner finishing the requeue derives the same delay. The store sets `not_before` from database time.
  - After `max_attempts`, or on an error that is not retryable, the job fails with `failure {error, kind: attempts_exhausted | harness_error}`.
- **Release on shutdown** goes back to the queue through checkpoint, event, save, and is not a failed attempt.
- **Intervals** come from `config/worker.yaml` (`WorkerSettings`): lease 30 s, heartbeat 10 s, poll 1 s ± 100 ms, 3 attempts, backoff 5 s doubling to 60 s.
  - Always: heartbeat ≤ lease / 3, poll < heartbeat, backoff max ≥ min.
  - Outside the Test environment, also: lease ≥ 10 s, heartbeat ≥ 1 s, poll ≥ 200 ms.
  - Each worker spreads its polls by a small generator seeded by its id, so workers started together do not poll in step.

### Waking

- **Polling only.** An idle worker tries a claim, then scans parked jobs. NOTIFY stays out of the job contract; it may later be an optional nudge inside the Postgres adapter.
- **What a scan reads.** For each parked job, it reads only `approval_answered` and `cancel_requested` (`EventLog::read_kinds`, indexed by run, kind and seq), after the larger of the stored `wake_checked_seq` and its own cached value. A match leads to `JobStore::wake` with `JobWake`, then the record. `wake_checked_seq` is written only by the fenced save that parks the job.
- **A cancel while running** is seen by a watcher on the run's log, which cancels the run's context; the loop stops with `cancelled`.
- A parked job holds no lease and makes no model call, and its wall time does not run.

### Environments

`contracts::Environment` is `Local`, `Test` or `Production`, from `SYNTH_ENVIRONMENT`; unset is Production, and the committed `config/` is production configuration. Every config section's `validate` receives it. A model provider's `base_url` may be plain http on a loopback host only in Test, which is how a worker process under test reaches a fake model server.

### Unfenced reasons

Each reason covers one operation across stores: `JobSubmit` the insert and `{absent → queued}`, `JobWake` the wake and `{waiting or suspended → queued}`. This replaces ADR-0032's "For PR 12" item 5, which asked for a new reason; see the ADR-0032 amendment. `crates/api` stays outside the `UnfencedReason` rule: in PR 14 bootstrap hands it a pre-built `ApiCommand` authority.

### Bootstrap and the worker

- **`crates/bootstrap`** (`synth-bootstrap`) is the composition root (§13.1). `build_worker` reads environment variables from an `Env` it is given and runs the startup checks, in order:
  1. the environment and the worker id;
  2. every file in `config/`, validated for the environment, then `prompts/`;
  3. `DATABASE_URL`, the database (migrated at startup), the Postgres stores, and `PgSdrRepository` into `SdrManager`, which only bootstrap builds;
  4. the tool servers of `worker.yaml` with their overlays;
  5. the registered checks, which must be exactly the ones `gates.yaml` requires;
  6. the agents, the context builder, and the model gateway with its keys;
  7. the orchestrator and the scheduler.
- **Startup failures.** Any failure is a `BootstrapError` naming its check. Bootstrap grants no write authority: the scheduler builds every context a run writes with.
- **TLS provider.** Bootstrap installs ring as the process's TLS crypto provider. reqwest is built without a provider (the model gateway picks ring in code), and the MCP transport's client needs the process default.
- **Test overrides.** Tests replace parts through `Overrides`: model providers by profile, tool servers, a wrapper around each store (SDR storage included), and the spawner.
- **`apps/worker`** (binary `worker [--config <dir>] [--prompts <dir>] [--id <id>]`).
  - The id comes from `--id`, else `SYNTH_WORKER_ID`, else `worker-<pid>-<start ms>`.
  - It logs to stderr through `tracing-subscriber`, filtered by `RUST_LOG`; no environment value is logged.
  - Ctrl-C, SIGTERM, Ctrl-Break or a console close stops claiming, releases the running job and drains the worker's tasks; a second signal exits at once.
  - Exit codes: 0 after a clean shutdown, 2 for the command line or a startup check, 1 otherwise.

## Alternatives considered

- **Record a transition right after its save.** The save that leaves running ends the fence, so the record could not be fenced, and a crash between the two would leave a state change nobody recorded.
- **A self-sufficient `job_state_changed` carrying the whole continuation.** It would put opaque and untrusted data in the log; events reference durable data instead (ADR-0032 amendment).
- **A checkpoint before every model call.** It costs a write per call and saves nothing a rebuilt context does not.
- **Keeping every checkpoint for replay.** It grows with every step; content-addressed context sections give exact replay at lower cost.
- **NOTIFY for waking.** It would bring a delivery guarantee into the job contract that polling does not need.
- **`if`/`then` schema rules for the outcome fields.** typify 0.8 cannot generate them; the tagged status is the type instead.
- **A process-wide runtime or spawner in library crates.** It hides tasks from shutdown and tests; the injected `Spawner` keeps them visible.

## Consequences

- Every transition out of running costs three writes. Every phase boundary costs a fenced save of the record.
- A crash during a non-idempotent tool call costs one repair attempt: the model sees `outcome_unknown`.
- An interrupted claim counts as a failed attempt, so a worker that crashes three times in a row on one job fails it.
- A stale worker that loses its lease mid-call leaves its effect in the tool's own store under the step's key; nothing in the harness references it.
- The system tests (`tests/system/`) run every crash point and scenario on Postgres, and `cargo xtask test-tools` kills a real worker process mid-run against the build123d server.

## Links

- Blueprint §3.4, §4.1, §5.4, §6, §7.2, §11.2, §13.1, §16.1
- ADR-0016, ADR-0020, ADR-0023, ADR-0026, ADR-0027, ADR-0029, ADR-0030, ADR-0032
- Increment 1 plan, PR 12
- `crates/runtime/src/scheduler/`, `crates/runtime/src/continuation.rs`, `crates/bootstrap/`, `apps/worker/`, `config/worker.yaml`, `schemas/jobs/continuation.schema.json`, `tests/system/`
