# ADR-0035: The replay store, SDR reads and benchmark evaluation

- Status: Proposed
- Date: 2026-10-26

## Context

Exit criterion 6 of Increment 1 is "`cargo xtask eval` runs the benchmark repeatedly and records the results as the single-loop baseline". Four things it needs were left open:

- **Exact replay.** ADR-0029 hashes what the model sees, and ADR-0031 keeps only the latest continuation, deferring exact replay to "content-addressed context sections in a replay store, a follow-up before baselines". Nothing stores what a call actually sent or received.
- **What a client can read.** API v0 serves runs, events, reports and artifacts. A benchmark outcome such as "L, W, T and mass are parameters of the final design" or "no committed version shows R4 passing" needs the design at each version, and the evidence its commit relied on.
- **Which rule asked for an approval.** The `approval_answered` outcome names an autonomy rule (`approve_final_design`). `approval_requested` records only the asking gate; the rule appears only in the free-text summary.
- **How outcomes are scored and a baseline is written.** ADR-0025 types the checks but leaves their evaluation to PR 15. Blueprint §16.4 asks for repeated runs and their spread, and §18 asks for a schema for every report.

## Decision

### The replay store

**One record per completed model call**, `ModelCallRecord` (`schemas/replay/model-call.schema.json`), keyed by the run and the seq of the call's `context_assembled` event.

- It holds:
  - the context hash, the model profile and id, and the output limit;
  - the system prompt and each block of each message, by hash;
  - the tool definitions, by hash;
  - the reply: its blocks by hash, its stop reason and its usage.
- **Blobs are content-addressed.** Each block, the system prompt and the tool list is stored once as its canonical JSON (`contracts::canonical`), keyed by the sha256 of those bytes. Postgres keeps the text as written (not `jsonb`, which would reorder keys). The instructions, and a design section that has not changed, are stored once however many calls send them.
- **The schema's `Block` mirrors `contracts::ContentBlock`.** The gateway's types stay hand-written. `contracts::replay` converts a request and its reply into a record with blobs and back, and a round-trip test pins the mapping.
- **`contracts::ReplayStore`** has a contract suite run in memory (testkit) and on Postgres (infra, migration `0006_replay.sql`). `put_call` stores a record and its blobs in one transaction; writing the same record again is Ok, and a different record under the same key, or a blob whose bytes do not hash to its key, is Conflict.
- **It is unfenced, like evidence (ADR-0032).** Records and blobs are immutable. A stale worker cannot obtain a new key, because the `context_assembled` seq comes from a fenced append; whatever it could still write is orphaned.
- **The runtime writes it** after the reply and before `model_call_completed`, so every `model_call_completed` has a record. A failed write fails the step, as a failed `context_assembled` append does. Failed calls are not recorded; their `error` event is the record.
- **Replay is checked against the log.** `context::context_hash` is public, so a reader rebuilds the request from a record and compares its hash with the logged `context_hash`.

### Reading the SDR through the API

`GET /v0/projects/{project_id}/sdr/versions/{version}` returns an `SdrVersionView` (`schemas/api/sdr-version.schema.json`): the document, its version record, and the evidence the record cites.

- 404 for an unknown project or version.
- `contracts::SdrViews`, beside `RunReports`, is implemented in `runtime` over `SdrManager` and the evidence store. The API still reaches the harness only through contract traits, and never touches SDR storage (blueprint §7.2).
- The route is not evaluation-only: the frontend will need SDR views (§3.5).

### Approval requests name their rules

`approval_requested` gains an optional `rules`: the IDs of the autonomy rules that matched. Governance writes it from the gate's matching rules. Events are written as schema 0.3; 0.1 and 0.2 events stay valid. This is an additive change to a core event (blueprint §4.1, item 1).

### Scoring

`evaluation::outcomes::score(problem, run)` is pure. `run` is a `RunRecord`: the run report, the event log, and the SDR version views from 0 to the final version. Each check is `pass`, `fail` or `undecided` (with the reason, when the record lacks what the check needs). An outcome passes only when every check passes.

| Check | Decided from |
| --- | --- |
| `stop_reason` | the report's stop reason is one of `allowed` |
| `parameters_present` | each ID is a parameter of the final document |
| `requirements_passing` | `contracts::requirement_status` over the evidence the final version cites is `pass` for each |
| `requirement_never_passing` | that status is not `pass` at any version |
| `parameter_matches_reference` | the expression over the final values, each converted to its declared unit, is within the relative tolerance of the parameter in `unit` |
| `approval_answered` | an `approval_requested` whose `rules` include the rule, and an `approval_answered` for that approval |

Requirement status uses the same rule as the `success` stop and the report, from check evidence only (§9.4). No score reads text the model wrote.

### The baseline

- **A schema:** `schemas/reports/baseline.schema.json`. A baseline records:
  - when it ran, the git commit, the agent, the model ids seen, the repetitions and the cost cap;
  - per run: its id, stop reason, outcome scores, usage and cost;
  - per problem: the pass rate of each outcome, and the mean, standard deviation, minimum and maximum of tokens, tool calls, wall time and cost.
- **`evaluation::baseline::summarize`** computes the summary; it is pure.
- **`cargo xtask eval`** drives the runs through `api-client` against a running api-server. It answers every approval with `approve` (the baseline says so), stops starting runs once the summed cost reaches `--max-cost-usd`, and writes `evals/baselines/<date>-<profile>/baseline.json`. A run's cost comes from `model_gateway::cost_usd` and the prices in `config/models/routing.yaml`.
- **`api_client::drive`** runs one request to its end: start, stream with resume, approve, then collect the report, the events and the versions. xtask and the system test share it.

## Alternatives considered

- **Only the context sections, without the reply.** This is the narrowest reading of ADR-0031, but the improvement loop (blueprint §9.2 "observe") would have to add the replies later, and a recorded run could not drive a provider in tests.
- **A snapshot of each job per call.** ADR-0031 already rejected keeping every checkpoint.
- **Blobs per section rather than per block.** The open turn's blocks would be stored again for every call of the turn.
- **Generated types for the gateway too.** Every adapter would change, and the gateway works today.
- **Scoring in-process against the stores.** The plan asks for a client that scores; an API route also serves the frontend.
- **Reading the rule from the approval summary.** That is free text, and parsing it would make the score depend on wording.
- **Scoring from the report's rows only.** The report is as of the final version; `requirement_never_passing` needs every version.

## Consequences

- Every model call costs one more write, and Postgres keeps the blobs of every run. Retention is not decided; nothing deletes them yet.
- A recording for `RecordedModelProvider` can now come from a live run's replay records, not by hand. That is a follow-up.
- The eval approves on the operator's behalf. Its baseline therefore measures the loop with approvals granted, not a human's judgement.
- An `undecided` check means the run lacked something to score (for example no final design). It counts as not passing in the summary and is shown separately.

## Links

- Blueprint §3.5, §4.1, §7.2, §8, §9.2, §9.4, §16.4, §18
- ADR-0021, ADR-0025, ADR-0029, ADR-0031, ADR-0032, ADR-0033, ADR-0034
- Increment 1 plan, PR 15b
