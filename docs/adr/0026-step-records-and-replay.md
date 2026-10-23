# ADR-0026: Step records and replay of tool calls

- Status: Proposed
- Date: 2026-10-23

## Context

Recovery must not repeat a side effect, and each effect carries an idempotency key that recovery checks before running it again (blueprint §6.3). The plan gives PR 8 "step records with idempotency keys, so recovery returns a completed call's stored result instead of calling again", and PR 12 relies on them to resume a killed worker. The job schema already says recovery uses step records, but no ADR or schema says what a step record holds or what a replay does in each state. A record keyed by the idempotency key alone also leaves three cases undecided:

- the same key sent with a different request;
- a call that started but whose outcome was never stored, because the worker died mid-call;
- a call that failed.

## Decision

- A step record (`schemas/jobs/step-record.schema.json`) is keyed by the idempotency key. It holds the run, step and tool, a sha256 of the canonical JSON of the tool name and arguments, `started_at`, and, once it finishes, `completed_at` with an outcome: `succeeded` (tool version, normalized result, evidence ids) or `failed` (error).
- `contracts::StepStore` stores step records. `begin` creates a started record atomically, so only one of several concurrent callers starts the step. `complete` writes the outcome once. The store sets both timestamps from its own clock (ADR-0023).
- The tool gateway treats an existing record as follows:

  | Record | The gateway |
  | --- | --- |
  | none | runs the pipeline and stores the outcome |
  | succeeded | returns the stored result, marked replayed: no tool call, no new evidence or events |
  | failed | returns the same failure, marked replayed; a retry uses a new idempotency key |
  | started, and the tool's overlay says `idempotent: true` | runs the call again and completes the same record |
  | started, and the tool is not idempotent | fails with `conflict` (outcome unknown) without calling the tool, and records that failure |
  | same key, different run, step, tool or request hash | rejects the call with `Conflict` |

- The step record is begun only after the gates allow the call, so a call that waited for approval is not mistaken for an interrupted one when it resumes.
- Evidence ids are derived from the idempotency key. A step that runs again after an interruption finds its evidence already stored and keeps it.

## Alternatives considered

- Key alone, without a request hash: a reused key would replay the wrong result without any error.
- Always re-run an interrupted step: breaks §6.3 for tools with side effects.
- Never re-run an interrupted step: an idempotent tool such as build123d would need a human every time a worker died mid-call.
- Do not store failures: a replay would call the tool again, so a recovered run could take a different path from the original.

## Consequences

- PR 11's query loop gives each retry its own idempotency key (for example, the attempt number in the step id).
- A non-idempotent tool interrupted mid-call needs the orchestrator to escalate. The gateway reports it as `conflict`.
- A replay writes no events. If a worker dies after completing a step but before appending `tool_call_completed`, the event is missing; PR 12's recovery has to account for that.

## Links

- Blueprint §6.3, §13.1 (retries are idempotency-aware), §14.2
- ADR-0016, ADR-0023
- Increment 1 plan, PRs 8 and 12
