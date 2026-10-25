# ADR-0033: Run report v0

- Status: Proposed
- Date: 2026-10-26

## Context

Increment 1 must show a run's report, with every requirement status linked to its evidence (exit criterion 5, blueprint §18.1). Four things were undecided before PR 13:

- where the report's model-written narrative comes from (ADR-0030, follow-up 6);
- where its `versions` come from: `HarnessVersions` was recorded only on SDR version records, so a run that committed nothing had no provenance of its own, and a run resumed on a newer build recorded only one build;
- whether the report is stored at the end of a run or built when asked;
- how a requirement's status follows from its evidence, beyond ADR-0030's success rule.

## Decision

**Narrative.** Report v0 has none, and `TaskResult.summary` stays empty.

- The future source is the agent's own terminal result: an optional `summary` on the finish or commit call, validated against the agent's result schema. Assembly copies it into `narrative`, marked `origin: model`.
- There is no model call in the report path. With delegation (Increment 3), each subagent returns its summary in its result contract, and the parent rolls them up.

**Provenance comes from the log.** Event schema 0.2 adds two optional fields:

- `context_assembled` names the context's prompts and skills and the agent spec, as `{id, version?, sha256}`.
- A claim's own `job_state_changed {→ running}` names `harness_build`, the build that runs that segment of the run. A lost earlier claim's record, which a reconcile adds, names none.

`RunHarness` (in `schemas/common/types.schema.json`) is the projection of the log: `harness_builds` in segment order, then `model_ids`, `prompts`, `skills` and `agent_specs` as sets. `contracts::provenance::fold` computes it.

**The continuation keeps it as a cache.** Continuation schema 0.2 replaces `model_ids` with `harness`.

- The runtime's forwarder folds every event it forwards, so a saved record's harness is the projection of the log up to the job's progress mark. Commit provenance reads it.
- 0.1 records stay readable and are never rewritten, since a job's status and the log name them by sha256. `model_ids` is a named list type, so a stored `[]` serializes back as stored. Runtime reads a 0.1 record's models into a harness, and the next save writes 0.2. The schema cannot tie a field to a version, so `continuation::harness` enforces it.
- SDR version records keep their own `HarnessVersions` for transition provenance.

**The report is built on demand.** `artifacts::ReportAssembler::assemble(run_id)` writes nothing. It reads, as of the run's final SDR version (the continuation's `head`, or the job's base version):

| Report field | Source |
| --- | --- |
| objective, budget used, stop reason | the job |
| requirements, decisions, assumptions | the SDR document at that version |
| requirement statuses and their evidence | the evidence named by that version's record |
| started and ended | the first `run_started`, and the last `job_state_changed` into the job's state |
| approvals | each `approval_answered`, with the gate of its `approval_requested` |
| versions | `provenance::fold` of the log |
| artifacts | the run's artifacts made at or before that version |

- A completed, cancelled or failed run's report is `final: true`. A suspended run's is `final: false` and reflects the run so far. A queued, running or waiting run has none (`Conflict`).
- The report carries `assembler_version`. `schema_version` remains the report schema's version.
- A report that breaks its schema, or the rules the schema cannot state, is an internal error and is never returned. Those rules are: a `reason` exactly when a status is unverified, and an `ignored_reason` exactly when a finding is not used.
- A later cache would be a memo keyed by run, report schema version and assembler version, with no contract change.

**One status rule.** `contracts::requirement_status(requirement, findings, required: Option<Fidelity>)` is pure, and both the success stop (`runtime::stop::unmet`) and the report call it.

- A counted `fail` fails the requirement. Otherwise it passes when at least one finding counts and every counted finding is `pass`. Otherwise it is unverified, with a reason: `no_evidence`, `check_error`, `undecided`, `below_fidelity` or `stale`.
- `error` never fails a requirement: a check that failed to run is not evidence against the design.
- A finding counts unless an element it relied on is stale, or it is below the required fidelity, or of unknown fidelity when one is required. `Fidelity` is L0–L4 (blueprint §5.1). In Increment 1 no evidence carries a fidelity and nothing is stale, so every finding counts.
- `waived` is reserved. It will come only from an approval-gate record naming the requirement and the SDR version, never from a model or an agent.
- A row lists every finding considered as `{evidence_id, verdict, used, ignored_reason?}`, in place of `evidence_ids`. Report schema 0.1 had never been produced, so it changed in place.

## Alternatives considered

- **Narrative from the final reply's text:** couples the report to conversation state, and is fragile on a turn that is only a tool call.
- **Narrative from a summary call after the stop:** not deterministic, needs recorded responses in CI, fails on `budget_exhausted` and `cancelled` stops, and pulls report logic into the runtime.
- **Versions from the last commit's version record:** wrong for a run that committed nothing.
- **Versions on `run_started`:** written once, so stale after a resume on a newer build.
- **Versions from the final continuation record:** recovery steps 0 and 1 (ADR-0031) must not change the record the job's status names, so a recovery-only segment's build reaches the log but not the cache. The report folds the log instead; a test in `runtime/tests/scheduler.rs` shows the difference.
- **A report stored at the end of the run:** needs a new store, a fenced write and a recovery case, and freezes a report bug into old runs.
- **`error` counts as fail:** confuses a tool failure with a design failure, and would trigger repair on infrastructure faults.
- **Migrate stored 0.1 continuation records:** rewriting them breaks the sha256 their job's status and the log name.

## Consequences

- Every run from now on is traceable to its builds, prompts, skills and agent specs, which regression tracing (§9.5) and per-sub-call attribution under delegation depend on. Events and records written before this change carry less, and are never backfilled.
- The report's statuses and the success stop cannot disagree.
- Report assembly depends on `contracts` and `sdr` only (§10.3). In PR 14 the API reaches it through `runtime`, since `api` may depend only on `contracts` and `runtime`.
- `artifacts::Metadata::describe` exists, but no tool produces artifacts yet (ADR-0030). A run's artifact list is empty until one does.
- `escalations` is always empty until delegation (ADR-0030).

## Links

- Blueprint §2.4, §2.7, §3.4, §5.1, §5.4, §9.5, §10.3, §18.1
- ADR-0020, ADR-0021, ADR-0030, ADR-0031, ADR-0032
- Increment 1 plan, PR 13; `crates/artifacts`, `crates/contracts/src/status.rs`, `crates/contracts/src/provenance.rs`, `schemas/reports/run-report.schema.json`
