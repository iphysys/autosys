# Engineering Standards and Repository Blueprint

> **Status:** Draft v2.1, a proposed revision of v1 for review  
> **Audience:** Developers, reviewers, maintainers, and coding agents (GitHub Copilot, Claude Code and similar)  
> **Scope:** Agentic system-synthesis platform: requirements in, verified design artifacts out  
> **Normative language:** **MUST**, **MUST NOT**, **SHOULD**, and **MAY** indicate requirement strength.

---

## 0. Revision Notes (v1 → v2)

| Area | v1 | v2 | Reason |
| --- | --- | --- | --- |
| Harness language | Python | Rust. Domain tools stay in Python and Julia. | Rust is confirmed for the harness. |
| Language boundary | Implicit | MCP for calls, JSON Schema for data. Nothing else crosses. | Keeps tools in their native ecosystems and isolates crashes. |
| Repository layout | Python package under `src/` | Cargo workspace (`crates/`, `apps/`, `xtask/`) | Crate boundaries turn dependency rules into compile errors. |
| Core vs. extensions | Registries and factories | A small fixed core plus four ranked extension mechanisms | "Minimal but extensible" without future revamps. |
| Long-running work | Recovery and cancellation only | Durable jobs, wake triggers, heartbeat, defined stop conditions | Required by the agenticSS notes. |
| Synthesis process | Not specified | Fidelity levels, alternatives as SDR branches, backtracking, a definition of "done" | A generic agent loop has none of these. |
| SDR | Validated transitions | Adds branching, typestate transitions, concurrency rules and an enforced single writer | Needed for parallel agents and trade studies. |
| Self-improvement | Tier II knowledge only | One gated promotion pipeline for all harness data | Harness-level self-improvement is a stated goal. |
| Context | One file | A dedicated crate; context is rebuilt from the SDR | Compaction and context spec was an open item. |
| Reports | Change report only | Adds a run report and an improvement report | Standard, machine-readable outputs. |

### 0.1 v2 → v2.1: retained frontend

The previous codebase (a single agent loop behind a frontend) is retired. Only its frontend code is kept; everything else is rebuilt to this blueprint (Appendix B).

| Area | v2 | v2.1 | Reason |
| --- | --- | --- | --- |
| Frontend | `apps/ui/` placeholder, stack open | Top-level `frontend/` holding the retained web app and BFF | The frontend is carried over, not rebuilt. |
| Languages | Rust, Python, Julia | Adds the frontend's JavaScript/TypeScript as a consumer of `schemas/` | Three languages now generate types from the same contracts. |
| API contract | `crates/api` only | Schema-first OpenAPI in `schemas/api/`; resumable event stream (§3.5) | The retained UI needs a stable, typed API that survives long runs. |
| BFF | Not considered | Migration adapter first, then thin or folded into `crates/api` (ADR-0017) | It absorbs the API change so the UI keeps working during the rebuild. |
| CAD rendering | Not covered | View generation becomes a worker that produces derived view artifacts | The old `cad_renderer/` is rebuilt; views are not canonical geometry. |
| Improvement scope | Rust source out of scope | All source out of scope, including `mcp_servers/` and `frontend/` | Closes a gap in §9.1. |
| Adoption | No UI in increments | The retained UI is connected in Increment 1 | Makes each increment visible end to end. |

---

## 1. Purpose

This document defines:

1. coding and design rules for the platform;
2. the standard repository layout;
3. the fixed core and the extension contracts for agents, skills, tools, prompts, models, knowledge sources, evaluators, benchmarks, and SDR domains;
4. the rules under which the harness may improve itself;
5. the minimum checks required before a change is accepted.

The goal is a system that is safe to extend, and safe to let improve, without repeatedly modifying its architectural core.

The intended architecture is:

> **A Rust harness that controls the lifecycle, model-controlled local reasoning, bounded specialist agents, SDR-mediated coordination, deterministic tool execution in sandboxed workers, evidence-based verification, and gated self-improvement of versioned harness data.**

---

## 2. Core Engineering Principles

### 2.1 Separate configuration, declarative assembly, and executable code

- All non-code assembly MUST live outside the implementation crates.
- Agent composition, enabled skills, tool access, model selection, budgets, prompts, policies, and deployment-specific values MUST be declared through configuration.
- Configuration MUST select and connect capabilities. It MUST NOT contain hidden business logic.
- Code MUST implement stable behavior and contracts. It MUST NOT hard-code a particular agent, skill, tool, project, model provider, or deployment.
- Secrets MUST NOT be stored in source-controlled configuration.

> **If a change only changes what is assembled, selected, enabled, or parameterized, it belongs in configuration. If it changes behavior or an invariant, it belongs in code.**

### 2.2 Keep the core small and fixed; extend by addition

- The core is a short, explicit list of contracts (see §4.1). Changing any of them MUST go through an ADR.
- Adding a new instance of an existing extension type SHOULD require:
  1. one new declarative resource or implementation package;
  2. one manifest or configuration entry;
  3. focused tests;
  4. no edits to the orchestrator, the query loop, or unrelated crates.
- Core crates MUST NOT contain growing `match` arms or provider-name branches for each new capability. Use traits, registries, factories, and adapters.

### 2.3 Keep architectural ownership explicit

| Owner | Owns |
| --- | --- |
| Orchestrator | Global task progression and capability selection |
| Agent / subagent | Bounded reasoning and local execution within its task contract |
| Skill | Reusable procedural guidance and its declared contract |
| Tool adapter | Invocation, protocol translation, normalized results |
| Scheduler | Durable jobs, wake-ups, heartbeats, continuation |
| Process model | Fidelity levels, alternatives, stopping criteria |
| Context manager | What each model call sees, and why |
| SDR manager | Authoritative design-state transitions (sole writer) |
| Knowledge system | Reusable knowledge indexing and retrieval |
| Evaluator / checks | Criteria, evidence production, verdicts |
| Gate | Enforcement decisions on proposed actions and transitions |
| Hook | Observing or extending a lifecycle event (never enforcing) |
| Artifact manager | Generated-output metadata, provenance, publication |
| Sandbox | Execution isolation and resource enforcement |
| Model gateway | Model-provider access, routing, provider adaptation |
| Improvement manager | Proposing, evaluating, promoting, and rolling back harness-data changes |
| API (`crates/api`) | The external HTTP contract and event stream; authentication and authorization |
| Frontend | Presentation and capturing user intent (requirements, approvals, interventions) |
| BFF | UI-specific aggregation and session handling only (ADR-0017) |

No component MAY silently assume another component's ownership.

### 2.4 Preserve authoritative truth

- The **SDR (Structured Design Representation)** is the authoritative structured representation of the design.
- Requirements, specifications, design entities, interfaces, constraints, analyses, decisions, assumptions, evidence, and traceability SHOULD be represented in or referenced from the SDR.
- Memory, chat history, session logs, retrieved documents, generated CAD files, kernel variables, and agent messages are **not** authoritative design truth.
- Agents, tools, and workers MUST NOT mutate SDR storage directly. All mutations MUST pass through the typed, validated SDR transition API (§7).
- Every accepted transition MUST record provenance: actor, timestamp, reason, parent version, evidence references, and the versions of the prompts, skills, and agent specs in use.

### 2.5 Models reason; deterministic systems execute and verify

- Models MAY interpret goals, propose plans, select capabilities, generate candidates, and synthesize explanations.
- Deterministic code MUST enforce schemas, authorization, resource limits, state-transition rules, and non-negotiable engineering constraints.
- Model-generated outputs MUST be treated as untrusted until validated.
- Semantic evaluation MAY use a model, but a model MUST NOT override a failed security, schema, geometry, or hard engineering gate.
- An agent's claim that something passed is not evidence. Evidence comes from checks (§16.2).

### 2.6 Start with one orchestrator; keep delegation shallow

- The initial design SHOULD use one main orchestrator.
- A subagent MUST exist only when bounded context, specialization, isolation, parallelism, or independent lifecycle provides a clear benefit.
- A subagent MUST receive an explicit task contract, bounded context, selected capabilities, budget, stopping condition, SDR read slice, SDR write scope, and result schema.
- Delegation depth SHOULD NOT exceed two (orchestrator plus one worker level) without an ADR. Design recursion (system → subsystem → component) MUST be stored as data in the SDR, not mirrored in agent nesting.
- Agents MUST coordinate through typed tasks, results, SDR references, and evidence, not through undeclared shared mutable state.

### 2.7 Artifacts remain editable and interoperable

- Generated engineering artifacts SHOULD be editable in downstream tools.
- Neutral and exchange formats SHOULD be retained alongside tool-native formats where practical.
- Artifact metadata MUST identify format, producing tool and version, source SDR version and branch, generation parameters, and evidence.
- A rendered image is evidence or a view, not the canonical geometry.
- View formats for the UI (for example glTF meshes or images) are derived artifacts generated from the canonical geometry. They carry the same source SDR version and are never edited or treated as the design.

### 2.8 Secure by construction

- Tool access MUST be allow-listed and scoped.
- Network, filesystem, process, CPU, memory, and execution-time boundaries MUST be enforced outside the model and outside the agent loop.
- Credentials MUST remain outside prompts, logs, artifacts, kernels, and source control.
- External content, tool output, generated code, and retrieved documents MUST be treated as untrusted input.
- Security and authorization gates SHOULD fail closed.
- Optional telemetry and non-authoritative enrichment MAY fail open when explicitly configured.

### 2.9 The harness improves through data, never through weights or runtime source edits

- Self-improvement operates on **versioned harness data**: prompts, skills, agent specs, knowledge, routing hints, and workflow definitions.
- The running system MUST NOT modify source code in any language, schemas, policies, evaluation assets, sandbox configuration, or model weights.
- Source code changes happen only through the developer loop: pull requests, which may be written by coding agents but are reviewed by humans (§9.6).

### 2.10 Verification before autonomy

- A capability that increases autonomy (delegation, parallelism, self-improvement) MUST NOT be enabled by default until the benchmark (§16.4) can show it helps.
- If a capability does not beat its baseline on the benchmark, it SHOULD be removed rather than kept "just in case".

---

## 3. Architecture

### 3.1 Planes

```text
Experience  (frontend/web -> frontend/bff, cli)
        |
        v
API  (crates/api: HTTP + resumable event stream; contract in schemas/api/)
        |
        v
Agent control plane
  orchestrator | query loop | task manager | scheduler (durable jobs) | recovery
        |
        v
Capability plane
  agents (config) | skills | registries | process model | context manager
        |
        v
Tool and action plane
  tool gateway -> MCP client -> sandboxed workers (CAD, simulation, analysis, search, kernel)
        |
        +----------------------+
        v                      v
Information plane          Assurance plane
  SDR | knowledge |          checks -> evidence -> gates
  memory | session log       evaluation | approvals | audit
        |                      |
        +----------+-----------+
                   v
             Artifact plane  (artifacts, run reports)

Improvement loop (cross-cutting, gated):
  traces -> candidates -> held-out evaluation -> review -> promotion -> rollback
```

### 3.2 Languages and process boundaries

| Component | Language | Runs as |
| --- | --- | --- |
| Harness: orchestrator, scheduler, SDR manager, gates, context, API | Rust | `api-server` and `worker` binaries |
| Domain tools: build123d, RCAIDE, knowledge-graph access, NL-to-FOL | Python or Julia | One MCP server per tool, each in its own sandboxed container |
| Persistent CAD kernel (proposed, ADR-0013) | Python | Sandboxed MCP server holding a per-task interpreter session |
| CAD view generation | Python | Sandboxed MCP server producing derived view artifacts (replaces the old `cad_renderer/`) |
| Web frontend (retained) | JavaScript/TypeScript, as in the existing code | Static or server-rendered web app in `frontend/web/`, outside the Cargo workspace |
| BFF (retained) | As in the existing code | Thin service in `frontend/bff/`, outside the Cargo workspace (ADR-0017) |

Rules:

- The harness MUST talk to tools only through MCP. Data crossing the boundary MUST conform to schemas in `schemas/`.
- The frontend and BFF MUST talk to the harness only through the HTTP API and event stream (§3.5). They MUST NOT read the SDR store, job store, or runtime data directly.
- The harness MUST NOT embed a scripting runtime (Python, Lua, JavaScript) and MUST NOT load native plugins at runtime. In-process extension, when needed, uses WASM components (§4.2).
- Python workers, the frontend, and the BFF MUST NOT contain harness logic (orchestration, scheduling, gating, SDR writes, computing requirement status). Workers compute and return results with evidence; the frontend presents what the API returns.

### 3.3 Dependency direction

Dependencies point inward toward stable contracts:

```text
schema-types   (generated from schemas/)
     ^
contracts      (traits and core types; no infrastructure)
     ^
domain and application crates
  sdr  process  context  knowledge  tools  evaluation  governance  artifacts  model-gateway
     ^
runtime        (orchestrator, query loop, task manager, scheduler)
     ^
improvement    (uses runtime to execute evaluation runs)
     ^
infra          (adapters implementing contract traits; depends on contracts and schema-types only)
     ^
bootstrap      (composition root; the only crate that sees everything)
     ^
apps/*         (thin binaries)
```

The allowed edges are listed in §10.3. `cargo xtask arch-check` MUST fail CI when an undeclared edge appears.

The frontend sits outside the Cargo graph. Its only dependencies on the harness are types generated from `schemas/` and calls to the HTTP API.

### 3.4 State categories

Keep these types distinct. Do not create a generic `state` map that mixes them.

| Category | Contents | Authoritative? | Lives in |
| --- | --- | --- | --- |
| Agent execution state | Plan, active subagents, loop counters, checkpoints | For the run | Job store |
| Job state | Objective, progress, pending work, remaining budget, wake condition | For the run | Job store |
| SDR state | Requirements, design definition, relationships, constraints, analyses, decisions, evidence links | **Yes, for the design** | SDR store |
| Session log | Append-only record of messages, tool calls, and events | No (audit and replay) | `runtime-data/sessions/` |
| World state | Files, CAD models, kernel variables, simulations, external systems | No | Workers and artifact store |
| Context | Task-scoped information assembled for one model call | No | Built per call |
| Memory | Retained operational information from prior activity | No | Memory store |
| Knowledge | Reusable information and methods (Tier I trusted, Tier II provisional) | Tier I only, as reference | `knowledge/` and runtime overlays |
| Harness data | Prompts, skills, agent specs, routing hints, workflows | Yes, for behavior | Git, plus project overlays |
| Artifacts | Externally consumable outputs | No (derived from SDR) | Artifact store |
| Evidence | Observations supporting or rejecting a claim | Yes, as observation | Evidence store |
| UI state | View state, filters, unsent drafts | No | Browser |

### 3.5 External API and event stream

The retained frontend and any future client use one external contract, owned by `crates/api`.

- **Schema-first.** The HTTP API is described by an OpenAPI 3.1 document in `schemas/api/` that references the JSON Schemas in `schemas/`. Rust handlers are checked against it by contract tests; the frontend's client and types are generated from it. Neither side hand-writes API types.
- **Resources follow the architecture, not the old single loop:** projects, runs (jobs), events, approvals, interventions, SDR views (requirements, branches, staleness), artifacts and their views, and reports.
- **Event stream.** Run events are delivered as server-sent events. Every event carries a per-run sequence number taken from the append-only session log, so a client that disconnects resumes from its last sequence number instead of losing events. Runs outlive connections; the UI MUST NOT assume one connection per run.
- **Commands are requests, not state edits.** Starting a run, answering an approval, cancelling, and intervening are API requests that become events or gate decisions inside the harness (§6.4). The API MUST NOT expose direct writes to the SDR or job store.
- **Versioned.** The API carries a version. It is part of the fixed core (§4.1), so a breaking change requires a new version and an ADR.

---

## 4. Core and Extension Model

### 4.1 The fixed core

These contracts are the system's stable surface. Changing any of them MUST go through an ADR:

1. Message and event types, including the lifecycle events hooks can observe.
2. The query loop and the event stream it emits.
3. The tool contract: descriptor, request, result, and error.
4. The task contract and the result contract.
5. The SDR transition API.
6. The job contract and the scheduler's wake and continuation semantics.
7. The extension manifest formats (agent, skill, tool, evaluator, benchmark).
8. The model gateway contract.
9. The external API and event-stream contract (§3.5).

Everything else is an extension.

### 4.2 Extension mechanisms, in order of preference

| Rank | Mechanism | Used for | Authored by | Trust controls |
| --- | --- | --- | --- | --- |
| 1 | Declarative data: config, prompts, skills, agent specs, policies | Most new capability | Team, or the improvement loop (non-policy data only) | Schema validation, review, evals |
| 2 | Out-of-process MCP servers | Tools in any language | Team | Sandbox, allow-list, typed I/O |
| 3 | WASM components (reserved) | In-process hooks, verifiers, context providers | Team, later the improvement loop | No ambient authority, fuel and time limits, explicit capabilities |
| 4 | Rust crates | New core behavior | Core team via PR | Review; ADR when a core contract changes |

Prohibited:

- loading native dynamic libraries as plugins;
- embedding a scripting interpreter in the harness;
- loading code from user- or agent-supplied paths outside a sandbox.

The WASM host crate is created only when the first in-process extension is actually needed (ADR-0011).

### 4.3 Where a new capability goes

| You want to add | Use | See |
| --- | --- | --- |
| A tool | MCP server in `mcp_servers/` and a governance overlay in `config/tools/` | §14.2 |
| A skill | Package in `skills/` | §14.1 |
| A specialist agent | `config/agents/` and `prompts/agents/` | §14.3 |
| A model provider | Adapter in `crates/model-gateway/` | §14.4 |
| An SDR domain | Schema in `schemas/sdr/` and validators in `crates/sdr/` | §14.5 |
| A check or evaluator | `crates/evaluation/` | §14.6 |
| A benchmark problem | `evals/benchmarks/` | §14.7 |
| An API endpoint or event type | `schemas/api/` or `schemas/events/`, then `crates/api/` | §3.5 |
| A UI view | `frontend/web/`, using generated types | §13.3 |

---

## 5. Synthesis Process Model

The process model lives in `crates/process/`. Level contents below are **placeholders** until the team agrees the design workflow (Open Decision 1).

### 5.1 Fidelity levels

| Level | Purpose | Typical tools | Exit gate |
| --- | --- | --- | --- |
| L0 Requirements | Formalize, check consistency, allocate to subsystems | NL-to-FOL formalizer, knowledge graph | Requirements parse, are consistent, and are allocated |
| L1 Concept and architecture | Functional decomposition; open alternative branches | Knowledge graph | At least one feasible architecture per branch |
| L2 Sizing and performance | First-order sizing per branch | RCAIDE, Julia/MTK | Margins positive on must-level requirements |
| L3 Detailed analysis | Higher-fidelity checks on surviving branches | Analysis tools | Evidence at required fidelity |
| L4 Embodiment and artifacts | Geometry, configuration, reports from the SDR | build123d, artifact generators | Geometry and interface checks pass |

### 5.2 The inner loop

Every design step runs the same loop:

```text
generate (model) -> execute (tool in sandbox) -> measure (checks produce evidence)
   -> validate (gates decide) -> commit to SDR, or repair (model)
```

- Repair attempts MUST be bounded per step. When the bound is reached, the step escalates: to the orchestrator for replanning, to a different branch, or to a human.
- Repair is local. It MUST NOT be used as a substitute for backtracking across levels.

### 5.3 Alternatives and backtracking

- Design alternatives MUST be SDR branches (§7.3), not parallel conversation threads.
- When a check invalidates an earlier decision, the SDR's dependency tracking marks dependent elements stale. The orchestrator then repairs on the branch, backtracks to an earlier version, or switches branch.
- Losing branches are archived, not deleted, with the reason recorded.

Reference scenario (use it to test any process change): the agent sizes the wing around a catalog battery. The battery then fails a thermal check, and the heavier replacement changes total mass. The wing sizing must be marked stale and redone on a branch, not silently kept or restarted from scratch.

### 5.4 Stop conditions

A run stops with exactly one of these reasons, recorded in the run report (§18.1):

| Stop reason | Condition |
| --- | --- |
| `success` | Every must-level requirement has passing evidence at its required fidelity, no hard gate is failing, and artifacts were published from the final SDR version. |
| `failure` | No branch can satisfy the must-level requirements within the explored space. |
| `budget_exhausted` | Token, cost, wall-time, or tool-call budget is used up. |
| `cancelled` | A user or operator cancelled the run. |
| `approval_required` | A gate returned `ask` and the run is waiting (resumable). |

---

## 6. Durable Execution

Runs may take hours or days and wait on external simulations. The scheduler lives in `crates/runtime/src/scheduler/` and is executed by the `worker` binary.

### 6.1 Job model

- Every unit of pending work is a persisted **job** conforming to `schemas/jobs/`.
- A job MUST persist: objective, progress, pending work, artifacts produced, remaining budget, SDR base version and branch, wake condition, and idempotency key.
- Job states: `queued`, `running`, `waiting`, `suspended`, `completed`, `failed`, `cancelled`.

### 6.2 Continuation and wake-up

- A job either continues immediately or waits for a wake condition: a timer, an event, or a tool completion.
- Long-running tools MUST return a job handle immediately and report progress through MCP progress notifications. Completion emits an event that wakes the waiting job.
- A waiting job MUST NOT consume model budget.
- Running jobs hold a lease renewed by heartbeat. A job whose lease expires is treated as interrupted and recovered.

### 6.3 Recovery

- After an interruption, the scheduler MUST reconstruct enough state to resume from the job store, the SDR version, and the session log. It MUST NOT depend on in-memory state or kernel variables.
- Non-idempotent side effects MUST NOT be repeated on recovery. Each effect carries an idempotency key, and recovery checks it before re-executing.
- A persistent kernel lost on restart is rebuilt by replaying its recorded script artifacts. Its variables are never relied on as state.

### 6.4 Human in the loop

- Pause, approve, and resume use the approval gate. A paused run is a `suspended` job and survives restarts.
- Users MAY intervene at any time. Intervention is delivered as an event to the run, not as a direct state edit.

---

## 7. SDR (Structured Design Representation)

### 7.1 Contract and code generation

- `schemas/sdr/` (JSON Schema) is the canonical contract.
- Rust types are generated into `crates/schema-types/` at build time (typify). Python types for workers are generated from the same files. Generated code MUST NOT be hand-edited.
- Schemas SHOULD use a plain, portable subset of JSON Schema. Constructs the generators cannot map cleanly (complex `allOf`/`oneOf` compositions) SHOULD be avoided.
- CI MUST fail if generated code drifts from the schemas.

### 7.2 Write path

- `crates/sdr/` exports `SdrManager` as the only way to change design state. The storage wrapper is `pub(crate)`.
- Only `bootstrap` constructs the SDR repository adapter, and only `SdrManager` receives it. `cargo xtask arch-check` MUST flag any other crate referencing the repository trait.
- Transitions use typestate: `Proposed` → `Validated` → `Committed`. An unvalidated transition cannot be passed to `commit`.
- Gates decide whether a validated transition commits (§15).

### 7.3 Operations

| Operation | Purpose |
| --- | --- |
| `read(slice, version)` | Scoped read for a task's context |
| `propose(transition, base_version)` | Submit a change against the version it was based on |
| `validate(transition)` | Schema, semantic, and gate checks; pass or fail with reasons |
| `commit` / `reject` | Gate decision; a commit creates a new version |
| `branch(from_version, name)` | Start a design alternative |
| `compare(branch_a, branch_b)` | Trade-study diff on parameters, margins, and evidence |
| `merge` / `archive(branch)` | Adopt the winner; keep losers for traceability |

### 7.4 Concurrency

- Optimistic concurrency: a proposal on a stale base version is rejected if it touches the same elements, otherwise rebased.
- Parallel subagents MUST be given disjoint write scopes at delegation time.

### 7.5 Engineering data rules

- Units MUST be explicit, machine-readable data on every quantity, and validated by `evaluation/checks/units`.
- Coordinate systems, tolerances, and reference frames MUST be explicit in CAD and engineering contracts.
- Decisions MUST carry rationale. Assumptions MUST be recorded as SDR elements so they can be invalidated.

### 7.6 Migrations

- Every schema change MUST bump the schema version and include a migration with tests in `crates/sdr/src/migrations/`.
- Commit granularity (per element, subsystem, or task) is Open Decision 2.

---

## 8. Context Management

Context management lives in `crates/context/`.

- Context is **assembled per call, not accumulated**. Each call's context is built from: the task contract, an SDR slice, selected skills, selected knowledge, and the recent relevant session events.
- Compaction MUST rebuild context from the SDR and job state. Summarizing chat history MAY be used only for non-authoritative narrative.
- Each context section has a token budget. Assembly MUST be deterministic and inspectable, and its hash MUST be recorded in the session log so runs can be replayed.
- Trusted instructions and untrusted content (tool output, retrieved documents) MUST be clearly separated and labeled.
- Every included item MUST carry its provenance (source, version) so a reviewer can see why the model saw it.

---

## 9. Self-Improvement

Self-improvement operates at the harness level only. It changes the data that configures agents, never model weights and never source code at runtime.

### 9.1 Scope

| MAY be changed by the improvement loop | MUST NOT be changed by the improvement loop |
| --- | --- |
| Prompts (`prompts/`) | Source code (`crates/`, `apps/`, `xtask/`, `mcp_servers/`, `frontend/`) |
| Skills (`skills/`) | Schemas (`schemas/`) |
| Agent specs (`config/agents/`), within policy bounds | Policies (`config/policies/`), including permissions, autonomy, gates, and budget ceilings |
| Knowledge (Tier II candidates toward Tier I) | Evaluation assets (`evals/`), checks, and its own evaluator |
| Routing hints and workflow definitions | Sandbox and deployment configuration |
| Memory | Model weights |

The allow-list of mutable paths is declared in `config/policies/improvement.yaml` and enforced in code. An agent spec change MUST NOT widen its own tools, permissions, or budgets.

### 9.2 Pipeline

| Stage | What happens | Owner | Output |
| --- | --- | --- | --- |
| Observe | Mine session logs and run reports for failures and repeated patterns | Improvement manager | Findings |
| Propose | Write a candidate change to harness data with a stated hypothesis | A proposer agent | Candidate in `runtime-data/improvement/candidates/` |
| Evaluate | Run dev and held-out benchmarks, repeated to account for run-to-run variance | Evaluation runs (not the proposer) | Improvement report (§18.2) |
| Review | Check for regressions and for encoded exploits or benchmark gaming | Human reviewer | Decision |
| Promote | Apply at the approved scope (§9.3) | Improvement manager | Versioned change |
| Monitor | Track the change's effect on later runs | Improvement manager | Run report fields |
| Roll back | Revert if later evidence shows harm | Improvement manager or reviewer | Revert record |

### 9.3 Promotion scopes

| Scope | Requirements | Where it lives |
| --- | --- | --- |
| Session | None beyond schema validity. Usable only inside the run that created it, in its sandbox. | `runtime-data/improvement/candidates/` |
| Project | Passes dev and held-out benchmarks with no hard-gate regression | `runtime-data/projects/<id>/overlay/`, layered over trusted data |
| Trusted (global) | Project-level evidence plus human approval of a pull request carrying the improvement report | The repository (`prompts/`, `skills/`, `config/agents/`, `knowledge/tier1/`) |

Tier II knowledge is any candidate or project-scoped item. Tier I is trusted. There is one promotion pipeline for all harness data, and promotion to Tier I is always a reviewed pull request.

### 9.4 Evaluation integrity

- The proposer MUST NOT evaluate its own candidate.
- The held-out benchmark MUST NOT be readable by any agent sandbox. The repository holds only its manifest and hashes.
- Benchmarks are versioned and hashed. A change to a benchmark is a code-reviewed change, never an improvement-loop change.
- Acceptance MUST require: improvement on the target metric larger than the measured run-to-run variance, over repeated runs; and no regression on any hard gate.
- Scores MUST be computed from evidence produced by checks, never from agent self-reports.
- Reviewers MUST look for candidates that encode shortcuts or exploits (for example, a skill that games a check). Prior art reports a case where a self-improving harness retained an exploit as a reusable skill (Appendix A).

### 9.5 Versioning and rollback

- All trusted harness data is in Git. Rollback is a revert.
- Project overlays are versioned in the project's runtime data. Rollback restores the previous overlay.
- Every run records the exact versions (hashes) of prompts, skills, and agent specs it used, so any regression can be traced to a change.

### 9.6 Two improvement loops

| Loop | Changes | Made by | Gated by |
| --- | --- | --- | --- |
| Developer loop | Source code, schemas, policies, evals | Developers and coding agents | Pull request review and CI |
| Harness loop | Prompts, skills, agent specs, knowledge, routing | Runtime agents | Benchmarks, then pull request review for trusted scope |

Both loops end in Git. Neither bypasses review for trusted changes.

### 9.7 When it is switched on

Self-improvement is designed in from the first increment, because prompts, skills, and agent specs are versioned data from day one. It is **enabled** only when the benchmark and a stable baseline exist (§24, Increment 4).

---

## 10. Repository Structure

```text
repo-root/
├── AGENTS.md                       # short rules for coding agents; links to this document
├── README.md
├── ARCHITECTURE.md
├── CONTRIBUTING.md
├── SECURITY.md
├── CHANGELOG.md
├── Cargo.toml                      # workspace: members, shared dependencies, lints
├── Cargo.lock
├── rust-toolchain.toml             # pinned toolchain
├── deny.toml                       # cargo-deny: licenses, advisories, banned crates
├── .env.example                    # variable names only; never real secrets
├── .gitignore
├── .pre-commit-config.yaml
│
├── config/                         # non-secret declarative assembly
│   ├── app.yaml
│   ├── logging.yaml
│   ├── environments/
│   │   ├── local.yaml
│   │   ├── test.yaml
│   │   └── production.yaml
│   ├── agents/                     # each spec references its prompt and result schema
│   │   ├── main_orchestrator.yaml
│   │   ├── requirements_analyst.yaml
│   │   ├── concept_sizing.yaml
│   │   ├── embodiment.yaml
│   │   └── evaluator.yaml
│   ├── tools/                      # governance overlay on MCP tools
│   │   ├── build123d.yaml
│   │   └── rcaide.yaml
│   ├── models/
│   │   └── routing.yaml
│   ├── policies/
│   │   ├── permissions.yaml
│   │   ├── autonomy.yaml
│   │   ├── gates.yaml
│   │   └── improvement.yaml        # paths the improvement loop may change
│   └── projects/
│       └── example_project.yaml
│
├── prompts/                        # versioned prompt resources
│   ├── system/
│   ├── orchestrator/
│   ├── agents/
│   ├── skills/
│   ├── evaluators/
│   ├── fragments/
│   └── README.md
│
├── schemas/                        # canonical, language-neutral contracts (JSON Schema)
│   ├── api/
│   │   └── openapi.yaml            # external HTTP API (OpenAPI 3.1); references the schemas below
│   ├── sdr/
│   │   ├── system.schema.json
│   │   ├── requirement.schema.json
│   │   ├── design.schema.json
│   │   ├── evidence.schema.json
│   │   ├── transition.schema.json
│   │   └── branch.schema.json
│   ├── tasks/                      # includes SDR read slice, write scope, base version
│   ├── results/
│   ├── agents/
│   ├── skills/
│   ├── tools/
│   ├── jobs/                       # job, wake condition, heartbeat lease
│   ├── events/
│   ├── replay/                     # model calls recorded for exact replay (ADR-0035)
│   ├── artifacts/
│   ├── reports/                    # run, improvement, change, and baseline reports
│   └── config/
│
├── knowledge/                      # Tier I only; Tier II lives in runtime-data
│   ├── tier1/                      # human-approved; read-only to agents
│   │   ├── methods/
│   │   ├── rules/
│   │   ├── ontologies/
│   │   └── standards/
│   └── manifests/
│
├── skills/                         # trusted skill packages only
│   └── cad_construction/
│       ├── skill.yaml              # single source of skill metadata
│       ├── instructions.md
│       ├── examples/
│       ├── resources/
│       └── tests/
│
├── crates/                         # the Rust harness (package names use the synth- prefix)
│   ├── schema-types/               # generated from schemas/; never hand-edited
│   │   ├── build.rs
│   │   └── src/lib.rs
│   ├── contracts/                  # traits and core types; no infrastructure dependencies
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agent.rs
│   │       ├── tool.rs
│   │       ├── task.rs
│   │       ├── result.rs
│   │       ├── event.rs
│   │       ├── evidence.rs
│   │       ├── artifact.rs
│   │       ├── job.rs
│   │       ├── storage.rs          # repository traits implemented by infra
│   │       └── error.rs            # stable error codes
│   ├── sdr/                        # SDR manager: sole writer
│   │   ├── src/
│   │   │   ├── lib.rs              # exports SdrManager only
│   │   │   ├── manager.rs
│   │   │   ├── transitions.rs      # typestate: proposed, validated, committed
│   │   │   ├── store.rs            # pub(crate)
│   │   │   ├── validation.rs
│   │   │   ├── branches.rs
│   │   │   ├── concurrency.rs
│   │   │   ├── invalidation.rs     # dependency tracking and staleness
│   │   │   ├── traceability.rs
│   │   │   └── migrations/
│   │   └── tests/                  # property tests on transitions
│   ├── process/                    # synthesis process model
│   │   └── src/
│   │       ├── levels.rs
│   │       ├── design_tree.rs
│   │       ├── alternatives.rs
│   │       └── stopping.rs
│   ├── context/                    # context assembly and compaction
│   │   └── src/
│   │       ├── builder.rs
│   │       ├── selection.rs
│   │       ├── compaction.rs
│   │       └── token_budget.rs
│   ├── runtime/
│   │   └── src/
│   │       ├── orchestrator.rs
│   │       ├── query_loop.rs       # returns a Stream of events
│   │       ├── task_manager.rs
│   │       ├── capability_router.rs
│   │       ├── cancellation.rs
│   │       ├── recovery.rs
│   │       ├── event_bus.rs
│   │       ├── scheduler/
│   │       │   ├── job_store.rs
│   │       │   ├── wake.rs
│   │       │   ├── heartbeat.rs
│   │       │   └── continuation.rs
│   │       ├── agents/
│   │       │   ├── registry.rs
│   │       │   ├── factory.rs
│   │       │   └── bounded_worker.rs   # loop types only; specialists are config
│   │       └── skills/
│   │           ├── registry.rs
│   │           ├── loader.rs
│   │           └── validator.rs
│   ├── tools/                      # client side of tool execution
│   │   └── src/
│   │       ├── registry.rs
│   │       ├── gateway.rs
│   │       ├── permissions.rs
│   │       ├── normalizer.rs
│   │       └── mcp.rs              # rmcp client
│   ├── evaluation/                 # checks produce evidence
│   │   └── src/
│   │       ├── evaluator.rs
│   │       ├── verdicts.rs
│   │       ├── evidence_store.rs
│   │       └── checks/
│   │           ├── schema.rs
│   │           ├── units.rs
│   │           ├── requirements.rs # formalized requirements via a solver
│   │           ├── geometry.rs     # decides on evidence returned by a Python worker
│   │           ├── engineering.rs
│   │           └── semantic.rs
│   ├── governance/                 # gates decide; hooks observe
│   │   └── src/
│   │       ├── hooks.rs
│   │       ├── gates.rs
│   │       ├── policy.rs
│   │       ├── approvals.rs
│   │       └── audit.rs
│   ├── knowledge/
│   │   └── src/
│   │       ├── retrieval.rs
│   │       ├── provenance.rs
│   │       ├── tiering.rs
│   │       └── graph.rs            # graph-store adapter interface
│   ├── artifacts/
│   │   └── src/
│   │       ├── manager.rs
│   │       ├── metadata.rs
│   │       ├── provenance.rs
│   │       └── reports.rs          # run report assembly
│   ├── model-gateway/
│   │   └── src/
│   │       ├── gateway.rs
│   │       ├── routing.rs
│   │       └── providers/
│   ├── improvement/                # self-improvement pipeline
│   │   └── src/
│   │       ├── observe.rs
│   │       ├── propose.rs
│   │       ├── evaluate.rs
│   │       ├── promote.rs
│   │       ├── overlay.rs
│   │       └── rollback.rs
│   ├── infra/                      # adapters; backends selected by Cargo features
│   │   └── src/
│   │       ├── persistence/
│   │       ├── messaging/
│   │       ├── sandbox/
│   │       │   ├── docker.rs
│   │       │   └── openshell.rs
│   │       └── telemetry.rs
│   ├── api/                        # HTTP API and resumable event stream; conforms to schemas/api/
│   ├── api-client/                 # typed client of the HTTP API for the CLI and xtask (ADR-0034)
│   ├── bootstrap/                  # composition root shared by all binaries
│   │   └── src/
│   │       ├── lib.rs
│   │       └── settings.rs
│   └── testkit/                    # fakes and contract-test suites for every trait
│
├── apps/                           # thin binaries
│   ├── api-server/
│   ├── worker/                     # runs scheduler jobs
│   └── cli/
│
├── xtask/                          # codegen (Rust, Python, TypeScript), arch-check, validation, evals, ci
│
├── mcp_servers/                    # domain tools; one sandboxed image each
│   ├── rcaide/
│   │   ├── pyproject.toml
│   │   ├── Dockerfile
│   │   ├── src/
│   │   ├── models/                 # generated from schemas/; never hand-edited
│   │   └── tests/
│   ├── build123d/
│   ├── knowledge_graph/
│   ├── julia_mtk/
│   ├── nl_to_fol/                  # planned
│   ├── cad_views/                  # derived view artifacts for the UI; replaces the old cad_renderer/
│   └── cad_kernel/                 # proposed persistent Python kernel (ADR-0013)
│
├── frontend/                       # retained from the previous codebase; outside the Cargo workspace
│   ├── web/                        # was frontend/app_ui/ui/codes/
│   │   ├── package.json
│   │   ├── public/
│   │   ├── src/
│   │   │   └── generated/          # API client and types from schemas/; never hand-edited
│   │   └── tests/
│   └── bff/                        # was frontend/app_ui/bff/; role set by ADR-0017
│
├── deploy/
│   ├── docker/                     # images: api-server, worker, each MCP server, web, bff
│   ├── compose/
│   └── policies/                   # sandbox and network policies
│
├── tests/                          # cross-component tests only; unit tests live with their code
│   ├── system/
│   ├── e2e/                        # browser-driven tests across UI, API, worker, and tool servers
│   ├── security/
│   ├── regression/
│   ├── golden/
│   └── fixtures/
│
├── evals/
│   ├── benchmarks/
│   │   ├── dev/                    # visible to proposers
│   │   └── heldout.manifest.json   # hashes only; problems stored outside the repo
│   ├── scenarios/
│   ├── rubrics/
│   ├── baselines/                  # single-loop baseline results
│   └── reports/
│
├── examples/
│   ├── minimal_project/
│   ├── add_a_skill/
│   ├── add_a_tool/
│   ├── add_an_agent/
│   └── add_an_sdr_domain/
│
├── docs/
│   ├── architecture/
│   ├── concepts/
│   ├── interfaces/
│   ├── operations/
│   ├── developer-guide/
│   └── adr/
│
└── runtime-data/                   # gitignored; no source code
    ├── projects/
    │   └── <project-id>/
    │       ├── sdr/                # versions and branches
    │       └── overlay/            # project-scoped promoted harness data
    ├── sessions/                   # append-only session logs
    ├── jobs/
    ├── artifacts/
    ├── evidence/
    ├── improvement/
    │   └── candidates/
    └── logs/
```

### 10.1 Repository rules

- `crates/` contains harness product code only. `apps/` contains thin binaries. `xtask/` contains developer automation and MUST call product APIs rather than duplicate product logic.
- `config/` holds non-secret assembly and policy. `prompts/` holds prompt templates. `schemas/` holds canonical contracts.
- `skills/` holds trusted skill packages only. Candidate skills live in `runtime-data/improvement/candidates/`.
- `knowledge/` holds Tier I only. Tier II MUST NOT appear in the source tree.
- `mcp_servers/` holds tool servers. Each is self-contained (own lock file, image, tests) and contains no harness logic.
- `frontend/` holds the retained web app and BFF. It keeps its own package manager, lock file, and tooling, and is not part of the Cargo workspace. It contains no harness logic, and its API types live only in `src/generated/`.
- The old `frontend/app_ui/ui/codes/` nesting is flattened to `frontend/web/` on migration. Moves MUST preserve Git history (Appendix B).
- `runtime-data/` MUST be excluded from source control except for explicit test fixtures.
- A crate or folder is created when its component is first built (§24), not in advance.

### 10.2 Crate or module?

Make something its own crate only when at least one holds:

1. a boundary must be enforced (for example `contracts`, `sdr`, `governance`);
2. heavy or native dependencies need isolating (for example `infra`, `evaluation`, `model-gateway`);
3. it must be independently swappable or testable.

Otherwise it is a module of an existing crate. This is why agents and the skill runtime are modules of `runtime`.

### 10.3 Allowed crate dependencies

| Crate | May depend on |
| --- | --- |
| `schema-types` | external serialization crates only |
| `contracts` | `schema-types` |
| `sdr` | `contracts` |
| `process` | `contracts`, `sdr` |
| `knowledge` | `contracts` |
| `context` | `contracts`, `sdr`, `process`, `knowledge` |
| `governance` | `contracts` |
| `tools` | `contracts`, `governance` |
| `evaluation` | `contracts`, `sdr`, `tools` |
| `artifacts` | `contracts`, `sdr` |
| `model-gateway` | `contracts` |
| `runtime` | `contracts`, `sdr`, `process`, `context`, `tools`, `evaluation`, `governance`, `artifacts`, `model-gateway` |
| `improvement` | `contracts`, `runtime`, `evaluation` |
| `infra` | `contracts`, `schema-types` |
| `api` | `contracts`, `runtime` |
| `api-client` | `contracts` |
| `bootstrap` | all of the above |
| `testkit` | `contracts`, `schema-types` (dev-dependency only) |
| `apps/*` | `bootstrap`, `api`, `api-client` |

---

## 11. Configuration Standard

### 11.1 Precedence

```text
defaults < environment file < project config < project overlay < environment variables < explicit CLI/API override
```

Secrets are resolved separately from a secret provider and MUST NOT be merged from ordinary YAML files.

### 11.2 Requirements

Every configuration object MUST:

- have a schema in `schemas/config/` and a typed Rust model;
- reject unknown fields (`#[serde(deny_unknown_fields)]`);
- declare a schema version;
- be validatable without starting the application (`cargo xtask validate-config`);
- produce actionable error paths;
- avoid arbitrary code execution;
- be immutable during a task. Declarative data MAY be reloaded between tasks.

### 11.3 Example agent spec

```yaml
schema_version: "1.0"
id: cad_specialist
kind: agent
implementation: bounded_worker
model_profile: engineering_reasoning
prompt: agents/cad_specialist.md
result_schema: results/cad_specialist.schema.json
skills:
  - cad_construction
  - geometry_verification
tools:
  - build123d
  - inspect_geometry
knowledge:
  - mechanical_design_rules
sdr:
  read_slice: from_task
  write_scope: from_task
limits:
  max_steps: 20
  max_tool_calls: 30
  timeout_seconds: 600
  max_repair_attempts: 3
```

The loader resolves names through registries. The orchestrator MUST NOT know concrete types for these entries.

---

## 12. Prompt Standard

Prompts are versioned software assets and a primary target of self-improvement.

- Prompts MUST be stored in `prompts/`, skill packages, or approved overlays, never as large string literals in Rust source.
- Each prompt MUST have one clear purpose and owner.
- Repeated fragments MUST be factored into named includes.
- Prompt assembly MUST be deterministic and inspectable. Variables MUST be explicit and schema-validated.
- Templates MUST distinguish trusted instructions from untrusted retrieved content.
- A prompt MUST NOT define permissions, bypass validation, expose secrets, or be the only enforcement of a hard constraint.
- The prompt version and hash MUST be recorded in run provenance.
- Prompt changes MUST have regression coverage. Changes from the improvement loop MUST carry an improvement report.

Recommended header:

```yaml
---
id: cad_specialist
version: 1.0.0
purpose: Produce bounded CAD construction proposals.
inputs:
  - task
  - sdr_slice
  - allowed_capabilities
output_schema: cad_specialist_result
owner: cad-domain
origin: human            # human | improvement-loop
---
```

---

## 13. Code Design Rules

### 13.1 Rust (harness)

**Contracts and traits**

- Define traits in `contracts` before implementations. Keep them small, provider-neutral, and expressed in domain terms.
- Traits used through registries (`Box<dyn Tool>` and similar) MUST be object-safe. The convention for async methods on trait objects is fixed by ADR-0015 and MUST be followed consistently.

```rust
// Shown with async-trait so the trait stays object-safe for registries.
// The final convention is fixed by ADR-0015.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &ExecutionContext,
    ) -> Result<ToolResult, ToolError>;
}
```

**Composition root**

- All dependency construction happens in `bootstrap`. Library crates MUST NOT create global clients, database connections, or providers at startup or through statics.
- Dependencies are passed explicitly. Test doubles implement the same traits (`testkit`).

**Registries**

- Registration is explicit and deterministic, in `bootstrap`. There is no import-time or path-based discovery.
- Duplicate identifiers MUST fail at startup. Lookup errors MUST list the missing capability and the available alternatives.

**Data models**

- Use precise types at boundaries. Avoid `serde_json::Value` for stable domain data.
- Use newtypes for identifiers (`ProjectId`, `TaskId`, `SdrVersion`) instead of raw strings.
- Separate commands, events, entities, results, and persistence records.
- Timestamps MUST include a timezone.
- Units are data on SDR quantities and validated at runtime. Compile-time unit types MAY be used inside pure calculation code.
- Serialization formats MUST be versioned.

**Errors**

- Library crates use typed error enums (`thiserror`). `anyhow` is allowed only in binaries and `xtask`.
- Errors MUST carry a stable code, a message, a retryable flag, and a correlation ID. Never discard the source error.
- Library code MUST NOT panic on recoverable conditions. `unwrap` and `expect` are denied by lint outside tests, except for documented invariants.
- Tool failures MUST be returned as observations, never converted into fabricated success.
- Retries are centralized, bounded, and idempotency-aware.

**Async and concurrency**

- Use `tokio`. Every await on external I/O MUST have a timeout.
- Cancellation flows through a token in `ExecutionContext`. Every long-running operation MUST honor it.
- Use bounded channels. Do not block the async runtime; use `spawn_blocking` for CPU-heavy work.
- The query loop emits a `Stream` of typed events. Hooks subscribe to events; they do not wrap the loop.
- Capabilities MUST declare whether they are read-only, mutating, idempotent, and concurrency-safe.

**Visibility and safety**

- `pub` only for the contract surface. Default to `pub(crate)`.
- `#![forbid(unsafe_code)]` in every crate unless an ADR grants an exception.
- Feature flags select backends only. They MUST NOT change core behavior.

**Logging and observability**

- Use `tracing` with spans carrying project, session, task, step, agent, tool, SDR version, and correlation IDs. No `println!`.
- Do not log secrets, credentials, full prompts by default, or sensitive artifact contents.
- Emit lifecycle events for: task start and end, model call, tool request and result, subagent completion, gate decision, SDR transition, job state change, artifact publication, improvement promotion, cancellation, and failure.

**Dependencies and toolchain**

- Toolchain pinned in `rust-toolchain.toml`. Shared dependency versions declared once in `[workspace.dependencies]`.
- Lints declared once in `[workspace.lints]`. CI runs `rustfmt` and `clippy` with warnings as errors.
- `cargo-deny` checks licenses, advisories, and banned crates. A new dependency MUST be justified in the pull request.

**Generated code**

- Produced from `schemas/` by `schema-types/build.rs` or `cargo xtask codegen`. Never hand-edited. CI checks for drift.

### 13.2 Python and Julia (tool workers)

- Each server is its own package with a lock file, an image, and tests.
- Data types MUST be generated from `schemas/`, not hand-written.
- Lint and type-check in CI (for Python: a linter, a formatter, and a strict type checker).
- Workers are stateless between calls, except the persistent kernel, whose state is never authoritative.
- Workers MUST NOT access the SDR store, schedule work, or make gate decisions.
- Long operations MUST report progress and support cancellation.

### 13.3 Frontend and BFF (retained code)

**Contract**

- The API client and types are generated from `schemas/api/openapi.yaml` into `frontend/web/src/generated/`. Hand-written API types are not allowed. CI fails on drift.
- The frontend's existing calls, shaped for the old backend, are moved to the generated client view by view. Until then, the BFF translates them into calls on the new API (ADR-0017). Components never call the harness directly.

**Presentation only**

- The UI MUST NOT compute requirement status, margins, stop reasons, gate outcomes, or SDR state. It renders values returned by the API and links to their evidence.
- CAD is displayed from derived view artifacts served by the artifacts API, with the source SDR version shown alongside. The UI never treats a view as the design.

**Long-running runs**

- The event stream is resumed from the last sequence number after a disconnect or page reload (§3.5).
- Job states (`queued`, `running`, `waiting`, `suspended`, `completed`, `failed`, `cancelled`) are shown as returned. A suspended run with an `ask` gate shows an approval prompt that is answered through the API.

**Untrusted content**

- Model output, tool output, and retrieved text are untrusted. Markdown and HTML MUST be sanitized before rendering; raw HTML from these sources MUST NOT be injected. A Content Security Policy MUST be set.

**Configuration and secrets**

- Everything shipped to the browser is public. Frontend configuration holds only public values (API base URL, feature flags). Model-provider keys and other credentials never reach the browser or the BFF's client-facing responses.

**Tooling**

- Lint, format, type-check (strict mode for TypeScript), and tests run in CI, invoked from `cargo xtask ci`.
- Dependencies are pinned in the lock file and audited in CI.

**BFF**

- Holds UI-specific aggregation, session handling, and the temporary adapter that translates old-style calls into the new API. It MUST NOT contain harness logic, hold run state beyond UI sessions, or access the SDR, job, or artifact stores directly.

### 13.4 Comments and documentation

- Comments explain intent, invariants, trade-offs, or non-obvious constraints, not what the code says.
- Architectural decisions belong in ADRs.
- Public APIs need concise doc comments with examples where misuse is likely.
- Temporary workarounds MUST include an issue reference or removal condition.

---

## 14. Extension Contracts

### 14.1 Add a skill

```text
skills/<skill_id>/
├── skill.yaml
├── instructions.md
├── examples/
├── resources/
└── tests/
```

`skill.yaml` MUST declare: ID and semantic version; purpose and applicability; required inputs and context; output contract; preconditions and stopping conditions; required and optional tools; required knowledge; permission requirements; evaluation criteria; owner; origin (`human` or `improvement-loop`); and lifecycle status.

A skill MUST NOT own scheduling, persistent global state, credentials, or SDR persistence.

### 14.2 Add a tool

A tool MUST provide: an MCP server in `mcp_servers/`; versioned request and result schemas in `schemas/tools/`; a governance overlay in `config/tools/` (permission scope, read/write classification, side effects, timeout, concurrency, idempotency, approval); normalized errors; audit metadata; and contract and integration tests.

Execution flow:

```text
resolve -> schema validate -> semantic validate -> pre-hooks (observe) ->
policy and approval gates -> concurrency decision -> sandbox execute via MCP ->
normalize result -> post-hooks (observe) -> checks produce evidence -> observation
```

### 14.3 Add a specialist agent

An agent spec MUST declare: role and bounded goal; input task schema; allowed context; model profile; selected skills and tools; SDR read slice and write scope; permissions and autonomy level; budget and repair limit; stopping criteria; result schema; and escalation behavior.

Agents MUST NOT receive all tools or all project context by default. A specialist is configuration, not a new loop implementation.

### 14.4 Add a model provider

Implement the model gateway trait as an adapter in `crates/model-gateway/src/providers/`. Provider-specific request and response types stay inside the adapter. The rest of the system sees normalized messages, usage, tool requests, stop reasons, and errors.

### 14.5 Add an SDR domain

Include: a versioned JSON Schema; generated types; semantic validators; transition rules; invalidation rules; a migration strategy; traceability rules; example instances; and contract and migration tests.

### 14.6 Add a check or evaluator

Declare: the claim or property evaluated; required inputs; whether the method is deterministic or model-based; the evidence produced; the verdict vocabulary; severity; the meaning of any confidence value; gate consequences; and known limitations. A boolean without evidence is insufficient for consequential design validation.

### 14.7 Add a benchmark problem

Include: the requirements; the expected checkable outcomes; the required fidelity level; time and cost budgets; and the split (`dev` or held-out). Held-out problems are added only by the benchmark custodian (Open Decision 7) and are never stored in the repository.

---

## 15. Hooks, Gates, and Policy

### 15.1 Definitions

- **Hook:** observes or extends a lifecycle event. It may record, enrich, or trigger bounded follow-up work.
- **Gate:** permits, denies, asks for approval, or rejects a proposed action or transition.
- **Check / evaluator:** produces evidence and a verdict.
- **Policy:** the rules a gate applies.

### 15.2 Rules

- Hooks MUST NOT become authorization mechanisms.
- Gates MUST return typed decisions: `allow`, `deny`, `ask`, `retry`, or `reject_transition`.
- Security, permission, and authorization gates MUST run before execution.
- Design acceptance gates MUST run before SDR commit and artifact publication.
- Post-action hooks MAY capture evidence and trigger recovery, but cannot undo an external side effect.
- Gate failures MUST be auditable.
- The orchestrator MAY repair, replan, or escalate after rejection, but MUST NOT override a hard denial.
- An extension authored by the improvement loop (for example a WASM verifier) MAY observe and produce evidence. It MUST NOT act as a gate until promoted to trusted scope through human review.

---

## 16. Testing and Evaluation

### 16.1 Test layers

| Layer | Where | Notes |
| --- | --- | --- |
| Unit | In-module `#[cfg(test)]` | Pure behavior and edge cases |
| Contract | `testkit` suites run against every trait implementation | One suite per trait |
| Property | `crates/sdr/tests/` and others | SDR transitions, concurrency, invalidation |
| Integration | Each crate's `tests/` | Adapter plus real or controlled dependency |
| Golden | `tests/golden/` (snapshot tests) | Prompt assembly, context assembly, structured outputs |
| System | `tests/system/` | Complete bounded workflows, including kill-and-resume |
| Security | `tests/security/` | Permissions, sandbox boundaries, untrusted input, secret leakage |
| Migration | `crates/sdr/` and config | Backward compatibility |
| Regression | `tests/regression/` | Every repaired defect |
| Worker | `mcp_servers/*/tests/` | Each tool server in isolation |
| API contract | `crates/api/tests/` | Handlers conform to `schemas/api/openapi.yaml`; event resume by sequence number |
| Frontend | `frontend/web/tests/` | Components and views against generated types and recorded API responses |
| End to end | `tests/e2e/` | Browser-driven: start a run, stream events, reconnect mid-run, answer an approval, view the run report |

### 16.2 CAD and system-synthesis verification

Separate these concerns:

1. schema and structural validity;
2. topological and geometry validity;
3. dimensional and unit correctness;
4. engineering and physics checks;
5. requirement satisfaction;
6. semantic resemblance and design intent;
7. artifact export and interoperability;
8. provenance and traceability.

A valid solid is not necessarily the right design. A visually plausible design is not necessarily physically or dimensionally valid.

### 16.3 Determinism

- Fix random seeds and store tolerances explicitly.
- No network access in unit tests. Model calls in tests use the `testkit` fake provider or recorded fixtures.
- Mark nondeterministic and model-based evaluations separately.
- Record model, prompt, skill, tool, configuration, and schema versions for reproducibility.
- Golden outputs MUST be reviewed, never blindly overwritten.

### 16.4 Benchmark

- The benchmark is a set of reference design problems with checkable outcomes, scored against a single-loop baseline.
- It MUST exist from Increment 1 (one problem) and grow to several problems by Increment 3.
- It has a `dev` split visible to proposers and a held-out split that is not (§9.4).
- Because model runs vary, comparisons MUST use repeated runs and report spread, not single scores.

---

## 17. Security Rules

- No secrets in Git, prompts, test fixtures, logs, kernels, or generated artifacts.
- TLS verification MUST remain enabled. Trust is established through approved CA configuration, not bypass flags.
- Tool workers run with least privilege, read-only mounts by default, dropped capabilities, resource limits, and default-deny network egress.
- The sandbox is a trait with replaceable backends. Docker is the first backend; OpenShell is evaluated as a backend (Open Decision 3).
- Shell access exists only inside sandboxed workers, bounded by an explicit command contract.
- File paths MUST be normalized and checked against allowed roots.
- External text MUST never be treated as system instructions.
- Generated code MUST be scanned and executed only in the sandbox.
- Human approval MUST be required for configured high-impact or irreversible actions, and for every promotion to trusted scope.
- Every state-changing operation SHOULD record actor, request, decision, result, and evidence in the audit trail.
- The API authenticates every request and authorizes it per project. Approvals and interventions record the human actor in the audit trail.
- The frontend sanitizes untrusted content, sets a Content Security Policy, and never receives credentials (§13.3). CORS is restricted to known frontend origins.

---

## 18. Reports

Every report has a schema in `schemas/reports/`. The JSON form is authoritative; any Markdown rendering is derived from it. Numbers and statuses come from evidence and the SDR. A model MAY write only the fields explicitly marked as narrative.

The frontend renders reports from the same JSON through the API. There is no UI-specific report format, and the UI MUST NOT recompute any reported value.

### 18.1 Run report (one per synthesis run)

- Run ID, project, objective, start and end times, stop reason (§5.4).
- Final SDR version and branch; branches explored and why each was archived.
- Requirements table: ID, priority, required fidelity, status (`pass`, `fail`, `unverified`, `waived`), evidence references.
- Key decisions with rationale; assumptions; open questions.
- Artifacts: ID, format, source SDR version, producing tool and version.
- Budget used: tokens, cost, wall time, and tool calls, per agent.
- Escalations and approvals.
- Versions: harness build, model IDs, and hashes of prompts, skills, and agent specs.
- Narrative summary (model-written, marked as narrative).

### 18.2 Improvement report (one per candidate)

- Candidate ID, type, and diff.
- Origin: the runs and findings that motivated it; the hypothesis.
- Dev and held-out results over repeated runs (mean and spread), against the current baseline.
- Hard-gate regressions (MUST be none to proceed).
- Review notes, including the exploit and gaming review.
- Decision, scope, reviewer, and rollback reference.

### 18.3 Change report (coding agents)

```text
Summary:
- <what changed>

Architecture:
- Owner/component: <crate or component>
- Contract used/changed: <contract>
- Config/schema impact: <impact>

Validation:
- <commands and results>

Risks or follow-ups:
- <remaining concern or "none">
```

---

## 19. Git and Change Management

### 19.1 Commits

- Keep commits small, coherent, and reviewable. Each SHOULD leave the repository buildable and testable.
- Do not mix formatting-only changes with behavior changes.
- Generated files identify their source and are committed only when the build requires it.
- Commit messages describe intent, not just modified files.

```text
<type>(<scope>): <imperative summary>

Why:
- reason for the change

What:
- key behavior or contract changes

Validation:
- commands/evaluations run
```

### 19.2 Pull requests

A pull request MUST state: the problem and intended outcome; affected crates and components; contract, schema, or config changes; security and permission impact; SDR or artifact compatibility impact; tests and evaluations performed; and rollback or migration considerations. Promotions from the improvement loop MUST attach the improvement report.

### 19.3 When an ADR is required

Create an ADR when a change affects: a core contract (§4.1); ownership or boundaries; protocols; persistence; security; extensibility mechanisms; public schemas; deployment topology; the self-improvement scope; or a major dependency.

---

## 20. Definition of Done

- [ ] Behavior is placed in the correct owner and crate.
- [ ] Public contracts are typed and versioned. No core contract changed without an ADR.
- [ ] Configuration validates and contains no secrets.
- [ ] No unrelated core crate was changed to register an extension.
- [ ] `cargo xtask ci` passes: format, lint, tests, `cargo-deny`, arch-check, schema and generated-type drift (Rust, Python, TypeScript), API contract tests, config and prompt validation, and frontend lint, type-check, and tests.
- [ ] Contract, integration, and system tests cover boundary behavior, including timeout, cancellation, retry, and resume after interruption.
- [ ] Security and permission implications were checked.
- [ ] SDR transitions and migrations are valid.
- [ ] Evidence and provenance are emitted where required.
- [ ] Prompt and skill changes have regression coverage; improvement-loop changes carry an improvement report.
- [ ] Generated artifacts are traceable to SDR version, configuration, and tool versions.
- [ ] Documentation is updated.

---

## 21. Instructions for Coding Agents

`AGENTS.md` at the repository root holds a short version of this section and links here.

Coding agents MUST follow this sequence:

1. Read `AGENTS.md`, the nearest crate README, the relevant contracts, and applicable ADRs.
2. Identify the owner and crate for the requested behavior (§2.3, §10.3).
3. Inspect existing traits, registries, schemas, configuration, and neighboring tests before writing code.
4. Make the smallest coherent change.
5. Extend through an existing contract. Do not add branches to the orchestrator or query loop for a new implementation.
6. Put assembly in `config/`, prompt text in `prompts/` or skills, contracts in `schemas/`, harness behavior in `crates/`, and presentation in `frontend/web/`.
7. Do not weaken validation, TLS, permissions, sandboxing, lints, or tests to make a change pass.
8. Do not invent unavailable APIs, fields, tools, or decisions. Mark unresolved assumptions explicitly.
9. Add or update tests with the implementation.
10. Run the narrowest relevant checks, then `cargo xtask ci`.
11. Report using the change report format (§18.3).

Coding agents MUST NOT:

- perform broad refactors unless required by the task;
- edit generated code, or hand-edit types that should be generated from `schemas/`;
- add `unsafe`, `unwrap` in library code, or new dependencies without justification;
- change the `contracts` crate or any core contract (§4.1) without an ADR;
- add a dependency edge not allowed by §10.3;
- put harness logic in a Python worker, the frontend, or the BFF;
- hand-write API types in the frontend, or call the harness other than through the API;
- change the API in `crates/api` without first changing `schemas/api/openapi.yaml`;
- put secrets or environment-specific paths in code;
- bypass the SDR manager for design-state changes;
- grant an agent spec unrestricted context, tools, or permissions;
- treat model output as validated truth;
- silently change a public schema or serialized format;
- modify `evals/`, policies, or benchmarks as part of an unrelated change;
- claim tests passed unless they were run.

---

## 22. Minimal Extension Examples

### 22.1 New tool

```text
1. Add schemas/tools/<tool>.request.schema.json and <tool>.result.schema.json
2. Implement mcp_servers/<tool>/ with generated types, Dockerfile, and tests
3. Add config/tools/<tool>.yaml (permissions, side effects, timeout, approval)
4. Add sandbox and network policy in deploy/policies/
5. Add contract and integration tests
```

No change to `runtime` or `tools` crates should be needed.

### 22.2 New skill

```text
1. Add skills/<skill>/skill.yaml and instructions.md
2. Add examples and resources as needed
3. Add validation and behavioral tests
4. Reference the skill from an agent spec
```

No query-loop change should be needed.

### 22.3 New specialist agent

```text
1. Reuse the bounded_worker loop type
2. Add prompts/agents/<agent>.md
3. Add config/agents/<agent>.yaml with only the required skills, tools, knowledge, and SDR scope
4. Define task and result schemas and budgets
5. Add routing criteria as configuration, and tests
6. Show on the benchmark that delegating to it beats the baseline (§2.10)
```

---

## 23. ADR Set

| ADR | Decision | Status |
| --- | --- | --- |
| 0001 | Record architecture decisions | Accepted |
| 0002 | Rust for the harness; Python and Julia for domain tool workers | Accepted |
| 0003 | JSON Schema is canonical; Rust, Python, and TypeScript types are generated from it | Proposed |
| 0004 | MCP is the only harness–tool boundary | Proposed |
| 0005 | Sandbox is a trait; Docker first, OpenShell evaluated | Proposed |
| 0006 | Single orchestrator; delegation depth at most two | Proposed |
| 0007 | SDR transitions are typed, versioned, and branchable; SDR manager is the sole writer | Proposed |
| 0008 | Hooks observe; gates enforce | Proposed |
| 0009 | Durable execution: job store, wake conditions, heartbeat leases | Proposed |
| 0010 | Self-improvement edits versioned harness data only, gated by held-out evaluation and human review | Proposed |
| 0011 | No embedded scripting or native plugins; WASM reserved for in-process extensions | Proposed |
| 0012 | STEP is the primary neutral CAD interchange format | Proposed |
| 0013 | Persistent Python CAD kernel as a sandboxed MCP server | Open |
| 0014 | Knowledge graph store adapter | Proposed |
| 0015 | Async methods on trait objects: `async-trait` or boxed futures | Open |
| 0016 | Persistence backend for SDR, job store, and session logs | Open |
| 0017 | Role of the retained BFF: migration adapter, then kept thin or folded into `crates/api` | Open |
| 0018 | Schema-first external API (OpenAPI 3.1 in `schemas/api/`) with a resumable server-sent event stream | Proposed |
| 0019 | Retain the previous frontend; rebuild everything else (Appendix B) | Accepted |

Each ADR MUST contain: context, decision, alternatives, consequences, status, and links to affected contracts.

---

## 24. Adoption Sequence

Crates and folders are created when their increment starts. Each increment ends with something working end to end.

| Increment | Builds | Exit criterion |
| --- | --- | --- |
| 1. Execution kernel | `contracts`, `schema-types`, `runtime` (loop and minimal scheduler), `tools` with MCP, SDR v0 (no branches), schema and units checks, run report, one benchmark problem. `api` with `schemas/api/` v0; retained frontend moved and connected through the BFF adapter (start a run, stream events, answer an approval, view the run report) | One benchmark problem runs end to end from the UI, survives a worker kill and a browser reconnect, and shows its run report |
| 2. Authoritative design model | SDR branching, invalidation, and migrations; `process` levels; backtracking; `cad_views` worker; UI views for branches, staleness, and CAD views | The reference scenario (§5.3) is handled by branching and invalidation, without restarting, and is visible in the UI |
| 3. Delegation and evaluation | Specialist agents, result contracts, disjoint write scopes, requirements check, 3–5 benchmark problems | Delegation beats the single-loop baseline on the benchmark, or it is dropped |
| 4. Reuse and self-improvement | `improvement` crate enabled, held-out split, project overlays, a second problem class | The core is unchanged across problem classes; harness edits are accepted only on held-out gains |

Cessna-class and UAV problems are both aircraft, so Increment 4 demonstrates cross-class reuse. A cross-domain claim needs a non-aero problem.

---

## 25. Open Decisions

1. **Design workflow:** the content of each fidelity level and how requirements flow down. Blocks filling in `process`.
2. **SDR v0 scope and commit granularity:** per element, subsystem, or task.
3. **Sandbox scope:** does OpenShell (or the chosen backend) wrap only tool workers, or the whole harness?
4. **Persistent kernel:** adopt the `cad_kernel` server (ADR-0013), or keep stateless tool calls only.
5. **Async trait convention** (ADR-0015).
6. **Persistence backend** for SDR, jobs, and session logs (ADR-0016).
7. **Benchmark custody:** who holds the held-out split, and where.
8. **BFF role** (ADR-0017): keep it thin once the old-backend adapter is gone, or fold it into `crates/api`. Decide after Increment 1, based on what the BFF still does.
9. **Authentication model** for the API and frontend: identity provider, session handling, and per-project authorization.
10. **Data from the previous system:** which knowledge-graph content and past SDR instances are worth migrating (Appendix B).

---

## 26. Review Heuristics

Reject or redesign a change when:

- adding a tool requires editing the orchestrator or `runtime`;
- adding an agent requires a new loop implementation;
- prompt text is scattered through Rust source;
- business logic is hidden in YAML or shell scripts;
- configuration is read globally from every crate;
- a model decides whether its own action is authorized;
- anything other than the SDR manager writes design state;
- memory, session logs, or kernel variables become a second source of design truth;
- a crate gains a dependency edge not listed in §10.3;
- harness logic appears in a Python worker, the frontend, or the BFF, or an interpreter is embedded in the harness;
- the frontend hand-writes API types, computes a status the API should return, or renders untrusted content unsanitized;
- an API change lands in `crates/api` without a matching change to `schemas/api/`;
- the UI loses events or progress when the browser reconnects during a long run;
- a job cannot resume after the worker is killed;
- an artifact is published without its SDR version and provenance;
- retries can repeat a non-idempotent side effect;
- parallel agents can overwrite the same state silently;
- a self-improvement candidate touches its own evaluator, policies, benchmarks, or budget ceilings;
- an improvement is accepted on dev scores alone, or on a single run;
- a run report contains numbers not backed by evidence;
- tests cover only successful paths.

Prefer a design when:

- the core stays fixed while capabilities are added;
- contracts are small, typed, and provider-neutral;
- configuration explains assembly without implementing behavior;
- state ownership and mutation paths are obvious;
- failures are structured and recoverable;
- evidence accompanies every important design claim;
- outputs remain editable, traceable, and reproducible;
- a capability's value is shown on the benchmark.

---

## 27. Document Maintenance

- Keep this document normative and relatively stable.
- Put explanations and tutorials in `docs/developer-guide/`, component rules in crate READMEs, and exceptions in ADRs.
- Review this document when extension mechanisms, trust boundaries, authoritative-state rules, or the self-improvement scope change.
- Do not copy these rules into multiple files. Link here and add only local constraints. `AGENTS.md` is the one permitted summary.

---

## Appendix A. Prior-Art Lessons

These notes come from public documentation, papers, and blog posts, not from reading source code. Vendor-reported performance figures have not been independently verified.

**pi (minimal TypeScript coding agent).**

- Stays stable through a small set of fixed contracts (message types, an event bus with typed hooks, an append-only session tree, a single extension API), with features such as MCP, sub-agents, and plan mode built as extensions.
- Adopted here: the fixed core (§4.1), the event stream with observing hooks, and the append-only session log.
- Not adopted: in-process TypeScript extensions, which have no safe Rust equivalent (replaced by declarative data, MCP, and reserved WASM); permissive default permissions.

**Prime Agent (Prime Intellect; a fork of pi rewritten in Rust).**

- A Rust host with a persistent Python kernel and recursive sub-agents.
- Its self-improvement step edits harness-level state: prompts, memories, skills, sub-agent specs, and workflows.
- It reportedly removed pi's TypeScript extension system in the Rust rewrite.
- Its self-improvement has no built-in verifier gate. Its published materials describe a case where an exploit was retained as a reusable skill.
- Adopted here: the Rust host and Python kernel split, and harness-data-only self-improvement.
- Added: a verifier gate, a held-out split, repeated runs, and human review before trusted promotion (§9).
- Not adopted: conversation-centric state (the SDR is authoritative) and deep recursive sub-agents (§2.6).

**References**

- Prime Agent announcement: https://www.primeintellect.ai/blog/prime-agent
- Prime Agent: A Self-Improving RLM Harness: https://arxiv.org/html/2608.23552v1
- Continual Harness: Online Adaptation for Self-Improving Foundation Agents: https://arxiv.org/html/2605.09998v1
- Reflections on Trusting Trust, Revisited: Contaminating Self-Modifying AI Coding Agents with Poisoned Benchmarks: https://arxiv.org/abs/2609.17817
- pi coding agent: https://pi.dev
- typify (JSON Schema to Rust types): https://github.com/oxidecomputer/typify
- Official Rust MCP SDK (rmcp): https://github.com/modelcontextprotocol/rust-sdk
- NVIDIA OpenShell: https://docs.nvidia.com/openshell/about/overview

---

## Appendix B. Migration From the Previous Codebase

The previous system was a single agent loop behind a frontend. Its frontend code is retained; all other code is rebuilt to this blueprint (ADR-0019).

### B.1 Where each old folder goes

| Old path (`autonomous_design/src/`) | Fate | New home |
| --- | --- | --- |
| `frontend/app_ui/ui/codes/` (`src/`, `public/`, `package.json`) | **Retained**, moved and flattened | `frontend/web/` |
| `frontend/app_ui/bff/` | **Retained**, becomes the migration adapter | `frontend/bff/` (ADR-0017) |
| `agent_loop/agent_api/routers/` | Rebuilt | `crates/api/`, contract in `schemas/api/` |
| `agent_loop/agent_runtime/` | Rebuilt | `crates/runtime/` with `context`, `process`, `tools`, `governance` |
| `agent_loop/plans/` | Rebuilt; plans are no longer files | Job store and task manager (`crates/runtime/`); design structure in the SDR |
| `agent_loop/tests/` | Rebuilt | Tests beside each crate, plus `tests/` |
| `knowledge_graph/` | Code rebuilt; content migrated after review | `mcp_servers/knowledge_graph/`, `crates/knowledge/`, curated content in `knowledge/tier1/` |
| `cad_renderer/` | Rebuilt | `mcp_servers/cad_views/` producing derived view artifacts |
| `sdr/` | Rebuilt | `schemas/sdr/` and `crates/sdr/` |

### B.2 Code is rebuilt; data is migrated

Rebuilding the code does not mean discarding the data it produced. Review these before deleting the old system:

- **Knowledge-graph content** (COTS catalog, regulations ontology, cached results). Migrate what is still correct into `knowledge/tier1/` or the graph store, with provenance. Anything unreviewed starts as Tier II.
- **Past SDR instances and runs.** Convert useful ones to the new SDR schema as test fixtures and candidate benchmark problems. They can also seed the single-loop baseline in `evals/baselines/`.
- **Old SDR documentation.** Use it as input to `schemas/sdr/`; it records decisions already made about entities, rationale, and dependency invalidation.

### B.3 Steps

1. Tag the last commit of the old system (for example `legacy-final`) so it stays available for reference.
2. Create the new layout. Move the frontend with history preserved: `git mv` if the new layout lives in the same repository, or a history-preserving filter (such as `git filter-repo`) if it starts in a new one.
3. Point the BFF at the new API. The BFF translates the frontend's existing, old-style calls into calls on the new API, so views keep working while they are moved to the generated client one by one. The old backend is not run.
4. Delete the old backend folders once Increment 1's exit criterion is met from the UI.
5. Remove the BFF's adapter code once no view calls the old API, then settle ADR-0017.
