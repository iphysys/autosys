# ADR-0032: Lease fencing

- Status: Proposed
- Date: 2026-10-25

## Context

A worker claims a job with a lease and renews it by heartbeat (ADR-0016, ADR-0023). PR 12 lets a lease expire and another worker reclaim the job, so a run can outlive its process. The old worker may still be running: in a stalled process, in futures not yet dropped, or in a restarted process that reuses its worker id. Before this ADR, `renew_lease` and `save` checked the holder and the expiry, but nothing checked event appends, step records or SDR commits. A zombie worker could keep writing next to the new holder: duplicate tool calls, interleaved events, a commit on top of the new holder's.

The holder alone cannot tell the two apart: the same holder may reclaim its own expired job. Expiry alone is checked against a clock, which a write that started before the expiry does not see.

## Decision

**The fence.** Each claim raises the job's lease epoch by one and returns a `FenceToken { job_id, holder, epoch }`.

- The epoch is the store-managed column `jobs.lease_epoch` (migration `0003_lease_fencing.sql`), and an entry in testkit's in-memory `Leases`. It is not part of `Job` or `job.schema.json`, so the token is its only carrier.
- `attempt` also rises on each claim, but its meaning belongs to the job contract and may change; the fence does not depend on it.
- The token has no public fields and is not serializable: it is never persisted or sent. `JobStore::claim` returns `Claimed { job, fence }`, `#[non_exhaustive]` so PR 12 can add to it.

**Every write states its authority.** `WriteAuthority` is `Fenced(FenceToken)` or `Unfenced(UnfencedReason)`, and every store write takes `Option<&WriteAuthority>` as its last argument. `renew_lease` and `save` take `&FenceToken` by type. Reads, and `claim`, take none.

- `UnfencedReason` is the whole allow-list, a closed `#[non_exhaustive]` enum, and the store enforces it, failing closed:

  | Reason | Permits only |
  | --- | --- |
  | `JobSubmit` (was `JobInsert`, see Amendments) | `JobStore::insert`, and `job_state_changed` into queued without a `from` or a checkpoint |
  | `JobWake` | `JobStore::wake`, and `job_state_changed` from waiting or suspended into queued on a checkpoint |
  | `ApiCommand` | appending `approval_answered` or `cancel_requested` |
  | `ProjectCreate` | appending SDR version 0 |

- `contracts::authorize(authority, StoreWrite)` is the first thing every store does for a write: a fence to verify, a reason that permits the write, or `lease_lost` for no authority or a reason that does not cover it. Stores never name `UnfencedReason`.
- A new reason is a new variant, with a test, recorded in an ADR. Job-scoped writers never get one.
- A fenced `insert` or `wake` is accepted while the fence is current, which leaves room for child jobs. The store does not require the fence's job to own the run it writes to, which would rule out a child writing to its parent.

**The check, in the write's own transaction, on the database clock.** A fenced write's first statement is:

```sql
SELECT 1 FROM jobs
WHERE id = $1 AND lease_holder = $2 AND lease_epoch = $3
  AND state = 'running' AND lease_expires_at > autosys_now()
FOR SHARE
```

- `FOR SHARE` conflicts with the claim's `FOR UPDATE SKIP LOCKED` and with the holder's `UPDATE`s. Until the write commits, a reclaim skips the row, and a renewal or a `save` that leaves running waits. After a claim commits, no write under an earlier epoch commits.
- The lock order is always `jobs`, then the store's own rows.
- `insert`, step `begin` and `complete`, and the SDR head read and insert, become transactions for this.
- `renew_lease` and `save` match the epoch in their own `UPDATE`.
- The fence is checked before any other condition: a write stale in two ways reports `lease_lost`, not `Conflict`.
- The in-memory stores apply the same predicate under the `Leases` lock, with the job store's clock standing in for the database's.
- `EvidenceStore` and `ArtifactStore` stay unfenced (see Consequences).

**The error.** `ErrorCode::LeaseLost` (`lease_lost`), not retryable. `ErrorCode` becomes `#[non_exhaustive]`.

- Its source is a typed `StaleFence`: the fence, why it was refused (reclaimed, expired, not running, missing job), and the job's current epoch and holder. The message reads, for example, `fence worker-1@3 on job-1 refused: job-1 is at epoch 4, held by worker-2`. The Postgres stores also log it.
- It replaces `Conflict` for a lost lease in `renew_lease` and `save`, also for a missing job, which was `NotFound`. `Conflict` keeps its other meanings.
- `save` with another job's fence is `InvalidInput`: a caller bug, not a lost lease.
- The worker abandons the run. It never marks the job failed, and the error never becomes a model observation, a repair or a retry: it ends the run's stream with `Err` unchanged.

**How the authority travels.** `ExecutionContext` is `#[non_exhaustive]` and gets `ExecutionContext::builder(..).authorized_by(authority).build()` and `write_authority()`.

- `for_step` and the `with_*` refinements keep the authority, so sub-calls fail together when the fence goes stale.
- Every writer passes `ctx.write_authority()`: the gates, the evaluator, the context builder, the runtime's recorder, the tool gateway and `SdrManager`. The fence authorises a write; it is not data the write stores, so `NewEvent`, `NewStep` and the event schemas are unchanged.
- Hooks observe and never write (blueprint §15.2): a subscriber's context has no authority.
- A child job has its own row, epoch and root context. A parked parent keeps its epoch until its next claim, so a future wake-up can require it.

**The tool call.** The fenced `tool_call_started` append is the check before a tool runs. It is the write-ahead record of the call: its idempotency key and the fence check commit in one transaction. Only `ToolGateway::announce_started` makes the private `StartedCall` that proves it committed, and only `invoke_started(StartedCall)` calls `Tool::invoke`. The window after that commit is the one the idempotency key covers (ADR-0026); the `Tool` contract does not change. A future kernel or sub-call reaches tools only back through the gateway over MCP.

**Who may do what** (`cargo xtask arch-check`, text rules like the `SdrRepository` rule, paths compared by component):

| Name | Allowed in |
| --- | --- |
| `issue_for_claim` | `crates/contracts/`, `crates/testkit/`, `crates/infra/` |
| `authorized_by`, `UnfencedReason` | `crates/contracts/`, `crates/testkit/`, `crates/bootstrap/`, `crates/runtime/src/scheduler/` |
| `.invoke(` | `crates/tools/src/gateway.rs`, `crates/testkit/` |

- Only the scheduler, through bootstrap, turns a claim into a writing context. Neither exists yet; they are listed in advance.
- Other crates' tests get authority from `testkit::leases`: `LeasedJob` (a job claimed for a year, and its fenced context), `fenced_ctx`, and one helper per reason, `job_insert()`, `job_wake()`, `api_command()` and `project_create()`. testkit is only ever a dev-dependency, so product code cannot call them.
- `crates/api` does not get `UnfencedReason`. In PR 14, bootstrap hands it a pre-built `ApiCommand` authority, which it passes to `EventLog::append`. The allow list is not widened for it.

**Pausing a call in tests.** `FakeTool::before_answer(hook)` is the standard pause point for crash and race tests: lease lost mid-call, kernel killed mid-exec, parent waiting on a child. The hook gets the call's index and request after the call is counted, and may go on, wait or fail the call. What it needs from the world it runs in comes through a `Slot`, a `OnceLock` filled after the world is built that panics with its name when read too early.

## Alternatives considered

- **Reuse `attempt` as the epoch.** Its meaning belongs to the job contract and may change, for example a reset on checkpoint progress; the fence would change with it.
- **The epoch in the job document** (`job.schema.json` 0.2, `lease.epoch`). A persisted copy a caller could send back in `save`, and a schema bump that would collide with PR 12's.
- **The fence as a field of `NewEvent` or `NewStep`.** Mixes authorisation into stored data and changes the event schemas.
- **The whole context as an argument of every store method.** Ties the stores to `ExecutionContext` and gives them correlation ids, cancellation and future fields they must not act on.
- **An explicit fence-check call in the gateway before `invoke`.** A check that is not the write leaves a window between check and write; the fenced append closes it, and a type makes it unskippable.
- **Advisory locks.** Held per session, so a pool connection reused by another task carries them; they also need a key scheme.
- **`SERIALIZABLE` transactions.** Retries on every conflict, on every write, for a guarantee one row lock gives.
- **A check without a row lock.** A claim can commit between the check and the write.
- **Unchecked unfenced writes,** a flag rather than a reason the store checks. Any writer could claim to be unfenced; the allow-list would live in every caller.

## Consequences

- Every store write costs one indexed lookup on `jobs` and briefly delays a heartbeat behind writes in flight, bounded by the statement timeout.
- A claim can come up empty while a stale writer holds its share lock for a moment. The claim loop already retries; PR 12 polls.
- A stale worker can still `put` evidence and artifacts. Their ids derive from the step's key or the run, and nothing references them unless a fenced event or commit does, so they stay orphaned, never authoritative.
- Text rules: a `use … as` alias defeats them, the same weakness as the `SdrRepository` rule. Review has to catch one.
- Hooks cannot write until a hook needs to, which would be a new decision.

**For PR 12** (record these in ADR-0031):

1. The scheduler claims and builds the job's root context with `authorized_by(WriteAuthority::Fenced(claimed.fence))`; bootstrap wires it.
2. The heartbeat calls `renew_lease(&fence, lease)`. On `lease_lost` it cancels the lease token and drops the run, never recording `cancelled` or `failed`.
3. Checkpoints call `save(job, &fence)`, so the continuation is fenced too.
4. Submission uses `JobSubmit` (renamed from `JobInsert`), and `ProjectCreate` for SDR version 0.
5. The waker uses `JobWake`, which also permits its `job_state_changed {suspended → queued}`: superseded by the amendment "a reason covers one operation", instead of a new reason.
6. A claim that reconciles the log appends under the new fence; recovery and the repair of `tool_call_completed` and `sdr_transition` write under it too. The lock order stays `jobs`, then the event counter.
7. PR 12's migrations are `0004_job_status.sql` and `0005_scheduler.sql`.
8. `crates/api` is not added to the `UnfencedReason` rule: bootstrap gives it a pre-built `ApiCommand` authority (PR 14).

## Amendments

### PR 12: a job's status is one tagged type

The job contract (blueprint §4.1) changes with job schema 0.2. A job's state and the fields that state needs are one object, `status: JobStatus`, a `oneOf` keyed by a `const` `state` tag. It replaces the top-level `state`, `wake_condition` and `stop_reason`.

| `state` | Carries |
| --- | --- |
| `queued` | `checkpoint?`: present when a woken or requeued job resumes, absent after a submit |
| `running` | nothing |
| `waiting`, `suspended` | `wake_condition`, `checkpoint` |
| `completed` | `stop_reason` (`success`, `failure` or `budget_exhausted`), `checkpoint` |
| `cancelled` | `checkpoint` |
| `failed` | `failure`, `checkpoint` |

- **Why.** The rule "this state carries these fields" was to be schema `if`/`then`, which typify 0.8 cannot generate (`if/then/else schemas are not supported`). As a tagged union the rule is the type, in Rust (an internally tagged enum), Python (a union of `Literal`-tagged models) and TypeScript (a discriminated union); all three generators were checked.
- No `const` fields: typify maps one to untyped JSON. A stop reason the state implies (`approval_required` for suspended, `cancelled` for cancelled) is not stored; `contracts::stop_reason` gives it, and `contracts::job_state` gives the tag.
- `checkpoint` names the continuation record the status rests on, by checkpoint number and sha256. A transition a worker decided applies only if the job row holds that record.
- `job_state_changed.to` is a `JobStatus`, so what each transition carries is a type too. `from` stays a `JobState`.
- The stores keep a plain, indexed `state`: in Postgres a column generated from `document #>> '{status,state}'` (migration `0004_job_status.sql`), so a claim or a wake-up rewrites the status in the document and the index cannot disagree with it. The `wake_condition` column goes. The migration moves queued and running jobs and refuses any other, which has no checkpoint to name.
- `lease` stays a top-level field the stores manage.

### PR 12: a reason covers one operation, across stores

"For PR 12" item 5 asked for a new reason for the waker's `job_state_changed`. Instead, each reason names one operation and covers what it writes in every store: the job write and the event that records it. `UnfencedReason::permits(StoreWrite)` checks both; there are no per-store reasons.

- `JobInsert` is renamed `JobSubmit`, and permits the insert and `{absent → queued}`. testkit's `job_insert()` is now `job_submit()`.
- `JobWake` permits the wake and `{waiting or suspended → queued}`, on the checkpoint the job was parked on.
- Neither permits the other's transition, nor any transition a worker makes (claims, requeues, stops): those are job-scoped and fenced. The contracts matrix and the fencing suite test each pair on both backends.
- Unfenced submit is for top-level runs only (the API and the CLI). A child job is a fenced insert under its parent's fence, so a parent whose lease was reclaimed cannot spawn children.

### PR 12: waking needs no unfenced write of its own

The wake-up scan once had a `mark_checked` write, outside any lease, to remember how far it had read. It is gone.

- `wake_checked_seq` is written only by the fenced save that parks the job. A worker's own copy, advanced as it reads, is a cache: re-reading from the stored value is always safe.
- The scan reads only the event kinds its wake condition needs (`EventLog::read_kinds`, indexed by run, kind and sequence number), so progress events and other traffic never count toward a re-read. While a job is parked, only client commands reach its run's log.
- A parent waiting on a child will read the child's job state; nothing writes into another job's log, so that needs no authority either.
- If re-reads become costly under fan-out, a persisted checkpoint can be added inside the Postgres adapter later, with no contract change.

### PR 12: a fenced writer writes ahead

A save that leaves running clears the lease, so nothing can be appended under that fence after it. One pattern holds for every fenced writer: **a fenced write-ahead record first, then the effect.**

- A transition out of running is checkpoint, event, save: a fenced checkpoint save with the job still running; `job_state_changed {running → X}` naming that checkpoint; then the save into X.
- A tool call already follows the pattern: the fenced `tool_call_started` comes before `Tool::invoke`.
- Finishing a decided transition is idempotent and conditional. A claim that finds `{running → X}` without its save applies X only if the job row holds the checkpoint the event names, and never runs again the step that decided X. A save refused with `lease_lost` abandons the run; the new owner finishes the transition on claim.
- Future transitions follow the same pattern, for example child sub-calls and kernel sessions.
- Unfenced operations (submit, wake) are conditional store writes that can lose a race, so they act first and record after; the next claim adds a record that is missing.

### PR 12: events reference durable data only

An event may reference only data that is already durable: events carry references (a version and a hash), stores carry payloads.

- `job_state_changed` carries the new status, including the checkpoint's number and sha256, never the continuation record itself.
- No opaque or untrusted data goes into the log: no thinking blocks, no tool results.
- Future large state follows the same rule: kernel scripts and sub-call results are stored in their store and referenced from events.
- Untrusted text already in the log, the error messages in `tool_call_completed.error` and `error` events (PR 8, PR 11), is a follow-up.

### PR 12: the scheduler and bootstrap exist

The paths the "Who may do what" table listed in advance now hold code: `crates/runtime/src/scheduler/` builds every fenced context, and `crates/bootstrap/` builds the stores. `cargo xtask arch-check` also scans the system tests under `tests/`, which get authority only from `testkit::leases`. A tool hook pauses at a `testkit::faults::Pause` through `Pause::stop`.

## Links

- Blueprint §6.1, §6.2, §6.3, §7.2, §10.3, §13.1, §15.2
- ADR-0016, ADR-0023, ADR-0026, ADR-0030, ADR-0031
- Increment 1 plan, PRs 12a, 12 and 14
- `crates/contracts/src/fence.rs`, `crates/infra/src/persistence/fence.rs`, `crates/infra/migrations/0003_lease_fencing.sql`, `crates/testkit/src/stores/leases.rs`, `crates/testkit/src/leases.rs`, `crates/testkit/src/contract/fencing.rs`, `crates/runtime/tests/fencing.rs`
