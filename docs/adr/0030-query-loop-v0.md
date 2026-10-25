# ADR-0030: Query loop v0

- Status: Proposed
- Date: 2026-10-25

## Context

The query loop and the event stream it emits are core contract 2 (blueprint §4.1). PR 11 adds `crates/runtime`, which connects the parts PRs 6 to 10 built: the checks and gates, the tool gateway, the model gateway and context assembly. The blueprint fixes the inner loop (§5.2), the stop reasons (§5.4), the single writer of design state (§7.2), rebuilt rather than accumulated context (§8), and a `Stream` of typed events (§13.1). It leaves open:

- how the model reads and changes the SDR, when a tool server may hold no SDR access;
- when a tool-use turn ends;
- what counts as a repair, and what the run does when repairs run out;
- what exactly makes a run `success`;
- how each budget is counted and what each stop records;
- how a run waits for an approval and continues;
- how the stream is driven, and what it yields.

ADR-0029 also left PR 11 a list of obligations as the context builder's caller.

## Decision

**The orchestrator.** `Orchestrator::{new, run, resume}` runs a job's task with the agent the task names, in the one loop type, `bounded_worker`. Every component is built at startup and passed in; nothing in the runtime reads configuration. PR 12's bootstrap will build them; the tests do here.

- `AgentRegistry` holds the specs from `config/agents/`. It refuses, listing every problem and the alternatives (§13.1):
  - two specs with one id;
  - a prompt that is not in the `PromptLibrary`;
  - any skill, since there is no skill registry yet;
  - a tool that is neither offered by the tool gateway nor `sdr_read` or `sdr_propose`;
  - a `result_schema` that does not compile;
  - `on_repair_exhausted: escalate`, naming Increment 3. Every agent is top-level until delegation arrives, so there is nobody to escalate to.
- Each spec gets a `VersionedHash` (sha256 of its JSON) for provenance. `cargo xtask validate-config` builds the same registry (ADR-0029 follow-up 5).
- A task must satisfy `tasks/task.schema.json`, and its tools, skills and each budget limit must be within its agent's. A task that does not fit is refused.
- `Orchestrator::new` refuses commit gates that do not include the `sdr_commit` gate, since a commit would then need no passing checks.

**The SDR tools.** The model reads and changes the design only through `sdr_read` and `sdr_propose`. They are not MCP tools: a worker may hold no SDR access or harness logic (§3.2, §13.2), and only `SdrManager` receives the repository (§7.2). They do not go through the tool gateway either: it would store tool evidence about the SDR's own values, and a tool has no way to return `ask`. The model sees them as tool definitions, and the capability router runs them in process.

- `sdr_read` reads the task's read slice through `SdrManager::read`, at the head or at a version the model names. It records nothing. A version that does not exist is a `not_found` result.
- `sdr_propose` takes `reason`, `final` and `operations` from the model. The harness sets the rest of the transition:
  - `id`: `tr-` and a hash of the run and the call's step id, so the same call always proposes the same id;
  - `base_version`: the SDR version of the call's context;
  - `actor`: the agent;
  - `schema_version`.
- The pipeline is `propose`, `validate`, `Evaluator::evaluate`, then a per-run `GateRunner` with the shared `sdr_commit` and `approval.sdr_commit` gates and the task's `WriteScopeGate`:
  - `allow` commits, and `CommitInfo.harness` records the build, the model ids so far, the context's prompts and the agent spec's hash;
  - `deny`, `retry` and `reject_transition` reject;
  - an invalid transition is rejected without checks;
  - each of these appends one `sdr_transition`;
  - `ask` decides nothing and the run waits.
- A decided commit and its `sdr_transition` are written with the I/O timeout only, never dropped on cancellation.
- Neither tool gets a step record; the SDR's version history is their record.
- Their schemas are in `schemas/tools/sdr/`. A tool's description is its request schema's `description`, and the meaning of `final` lives there, so the prompts did not change.

**Tool definitions and results.**

- The model gets a definition of each tool the task lists, in the task's order. The `input_schema` is the canonical request schema with every `$ref` inlined into `$defs`, under the names `schema-types` uses.
- A name the task does not list is denied without running, and an `error` event (`permission_denied`) records it.
- A result's content follows `schemas/tools/tool-result.schema.json`:
  - `ok` carries the call's `step_id`, the `result` and the `evidence_ids` a design change cites as the source of a measured value;
  - `error` carries `ErrorInfo`;
  - `denied` carries the reasons.
- `is_error` is set for every result but a successful one and a commit. A rejected proposal is `ok` content with outcome `rejected` and `is_error`. Arguments that break an SDR tool's request schema are an `invalid_input` error.
- Content is passed raw; the context builder wraps it as untrusted.
- Ids: step `step-{n}` per model call, tool call `step-{n}.{k}`, idempotency key `{run_id}:{step_id}`, or `sha256:` and its digest above 128 characters. A retry is a later step, so it gets a new key (ADR-0026).

**The turn.** Each step builds its context with `ContextBuilder::build`, with the agent's prompt and the open turn, and appends nothing itself. Then it calls the model, appends `model_call_completed`, and calls `compare_usage`.

- The tool calls of a reply run in order. The tool gateway still applies its per-tool locks.
- A turn closes when the model replies without a tool call, or when one of its proposals commits. The calls after the committing proposal in the same reply are not run: the next call starts with an empty open turn, so their results could not reach the model.
- Within a turn the context is rebuilt for every call, the default of ADR-0029. The ignored live test `live_rebuilt_context_mid_turn` ran on 2026-10-25 against Claude Haiku 5.5: the first reply held a tool call and a thinking block, and the API accepted the rebuilt context of the turn's second call. So ADR-0029's fallback, pinning `at`, is not needed.

**Repairs.** Each of these counts one failure since the last commit:

- a tool call that fails or is denied;
- a proposal that is invalid or rejected;
- a reply that ends the turn without success, a cut-off reply and a refusal included.

A commit resets the count. `max_repair_attempts` counts repairs, so the run fails on the first failure beyond it. Whenever a turn closes, `job.pending_work` is set to what is unmet at the latest version, for example `requirement R4: no passing check at sdr version 3`, or `final design: not committed at sdr version 3`. The harness reads every requirement for this, not the task's read slice.

**Success** (§5.4). A run succeeds right after a `final: true` transition commits through every commit gate, approval included, provided:

- every must-level requirement of the committed version, and every id in `task.requirement_ids`, has at least one check finding from that commit's evaluation;
- every one of those findings is `pass`. `unverified` never counts.

The rule names no particular check. A model's claim counts for nothing: a reply without a tool call is a repair. A final commit that does not meet the rule commits and the run goes on.

The §5.4 clause "artifacts published from the final SDR version" is met trivially in Increment 1, since no tool produces artifacts. It becomes enforceable with PR 13 or the first tool that produces artifacts.

**Budgets.** The effective budget is the task's.

- **Tokens:** input plus output, from each response's usage. Before a call the loop stops if `used + context estimate ≥ max_tokens`, and caps `max_output_tokens` at `min(profile max, max_tokens − used − estimate)`. The profile's maximum comes from `ModelGateway::routing()`.
- **Tool calls:** every tool call the loop handles counts, the SDR tools and denied calls included. A call beyond the budget does not run. With none left, the loop also stops before the next model call, which could neither act nor commit.
- **Steps:** one per model call.
- **Wall time:** a monotonic clock started when the run starts or resumes, plus `job.budget_used.wall_time_seconds`. At the deadline the loop cancels the step's context and still awaits the calls in flight, so they end as cancelled and the gateway completes their step records.
- The tool definitions are counted into the context's `reserved_tokens` with the default profile's estimate. The four tools of `main_orchestrator` come to about 3,800 estimated tokens, `sdr_propose` alone about 2,200.

**Stops.** `run_stopped` carries the latest SDR version the run saw or made.

| Stop | When | Events the runtime appends |
| --- | --- | --- |
| `success` | The rule above | `sdr_transition`, `run_stopped` |
| `failure` | One failure beyond `max_repair_attempts` | `error` (`repair.exhausted`), `run_stopped` |
| `budget_exhausted` | A budget is used up, or the context does not fit | `error` (`budget.tokens_exhausted`, `budget.tool_calls_exhausted`, `budget.steps_exhausted`, `budget.wall_time_exhausted`, or `context_overflow` naming the section), `run_stopped` |
| `cancelled` | The run's context is cancelled | `run_stopped` |
| `approval_required` | A gate on a commit or a tool call asks | `run_stopped`; `GateRunner` appends `approval_requested` |

- The `budget.*` codes follow `schemas/results/examples/result.budget-exhausted.json`. None is retryable.
- A cancelled call, or a cancellation found anywhere in the loop, is `cancelled` if the run was cancelled and `budget_exhausted` if its wall time is up.
- **Harness failures are not stop reasons:** a store or component failure, a model error after the gateway's retries, an invalid task, a result that breaks the agent's result schema. The runtime appends `error` when it can and the stream ends with `Err`. No `run_stopped` is written; PR 12 fails or retries the job (§6.3).
- `job.stop_reason` is set for the four terminal stops; `approval_required` leaves it unset. `job.state` and `job_state_changed` are PR 12's.

**Approval and resume.** On `approval_required` the outcome carries a `Continuation`: the open turn, the step that asked with its results so far, the pending call, the calls not yet run, and the counters. `Orchestrator::resume(continuation, answer, ctx)` puts the pending call through the gates again with the answer:

- a proposal is validated, checked and gated again;
- a tool call goes to the tool gateway with the answer.

Then the run continues. The pending call was counted when it first ran. The continuation lives in memory; PR 12 persists it.

**Events and the stream.**

- The runtime appends only `run_started` (new runs only), `model_call_completed`, `sdr_transition`, `error` and `run_stopped`, with actor `system/runtime`. Every other event comes from the component that owns it.
- `model_call_completed` gets no cost field (ADR-0027 left this to PR 11). `Budget` has no cost limit, run report v0 has no cost field, and the gateway already logs cost.
- `RunStream` implements `futures_core::Stream<Item = Result<RunEvent, RuntimeError>>`. `RuntimeError` maps onto `contracts::Error` with a stable code and a retryable flag. The stream yields:
  - `RunEvent::Recorded(Event)` for every event of the run, read back from the `EventLog` after each phase in sequence order, so events other components append are included and gaps are an error;
  - then `RunEvent::Stopped(Box<RunOutcome>)`.
- `RunOutcome` holds the stop reason, the `TaskResult` checked against the agent's `result_schema`, the updated `Job`, and the `Continuation` if any. The result's `summary` is left out: nothing decides yet which model text would be one.
- The stream is driven by polling, with no spawned task. The run sends events through a bounded channel of 64, so a slow reader slows the run. Dropping the stream drops the run, as a crash would.
- `config/runtime.yaml` (`io_timeout_seconds: 10`, schema `config/runtime.schema.json`) bounds every read and write the loop makes itself. Tool and model calls keep their own timeouts.

## Alternatives considered

- **`sdr_read` and `sdr_propose` as an MCP server.** Puts SDR access in a worker (§3.2, §13.2) and needs the repository outside `SdrManager` (§7.2).
- **Through `ToolGateway`.** It would store tool evidence about the SDR's own values, and `Tool::invoke` cannot return `ask`.
- **Keep the turn open across commits.** The context of the next call would mix a design version the model has not seen with tool results from an older one. Rebuilding from the SDR is what §8 asks for.
- **Run the calls after a commit.** Their results could not reach the model, so the calls would spend budget for nothing.
- **Let the model end the run with success.** Breaks §2.5: a claim is not evidence.
- **Escalate on repair exhaustion.** There is no agent above the orchestrator until Increment 3.
- **A spawned task feeding the stream.** Library crates may not spawn; the caller decides where the run runs. Polling also ties the run's lifetime to the stream's.
- **A cost field on `model_call_completed`.** No budget or report reads it yet.
- **Pin the turn's snapshot.** Needed only if the API refused a rebuilt context mid-turn; the live test showed it does not.

## Consequences

- The loop is `RunStream<'_>` over a borrowed `Orchestrator`. PR 12 drives it from the worker; it is `Send`.
- A crash at any point loses the in-memory continuation. Until PR 12 persists it, an approval survives only within the process.
- A commit whose timeout fires mid-write leaves its outcome unknown and ends the run as a harness failure. The transition's id is deterministic, so PR 12 can find out whether it committed.
- Evidence of a proposal that asked is recorded again when it is re-evaluated on resume.
- Nothing yet checks a parameter's value against the evidence it cites. A model could cite an evidence id it never received, or a value no tool reported, and the numeric check would still pass. The benchmark's `parameter_matches_reference` outcome catches it in PR 15; a provenance check in `evaluation` is a follow-up.
- A provider that refuses a prompt as too long still arrives as `invalid_input`, a harness failure, until ADR-0029 follow-up 1 maps it to `context_overflow`.
- `max_tokens` 1000000 and `max_steps` 40 in `main_orchestrator.yaml` are provisional (ADR-0025).

**Follow-ups, left for PR 12**

1. Persist the continuation (the open turn with its opaque thinking blocks, the pending call, the counters) and the job, with a schema.
2. Detect a transition that committed before a crash, by its deterministic id.
3. Account for the `tool_call_completed` a replay does not write (ADR-0026).
4. Job states and `job_state_changed`, waking on `approval_answered` and `cancel_requested`, and bootstrap.

**Other follow-ups**

5. A provenance check in `evaluation`: a parameter's value and source must match the evidence it cites.
6. Where the result's narrative `summary` comes from (PR 13).

## Links

- Blueprint §2.4, §2.5, §2.6, §3.2, §3.5, §4.1, §5.2, §5.4, §7.2, §8, §11.2, §13.1, §15.2
- ADR-0015, ADR-0020, ADR-0021, ADR-0024, ADR-0025, ADR-0026, ADR-0027, ADR-0028, ADR-0029
- Increment 1 plan, PR 11; `crates/runtime`, `config/agents/main_orchestrator.yaml`, `config/runtime.yaml`, `schemas/tools/sdr/`, `schemas/tools/tool-result.schema.json`, `governance::WriteScopeGate`
