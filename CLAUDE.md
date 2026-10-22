# autosys

Agentic system-synthesis platform: requirements in, verified design artifacts out. A Rust harness runs an orchestrator agent that calls engineering tools (Python/Julia MCP servers in containers) and records the design in the SDR (Structured Design Representation).

## Read before any work

1. `docs/architecture/blueprint.md`: the engineering standard. Normative; it wins over anything else.
2. `docs/plans/increment-1.md`: the current plan, split into PRs with done-when tests.
3. `docs/adr/`: decisions already made. Do not re-decide them.

## How to work

- One PR from the plan per session. Implement only that PR's scope.
- Start in plan mode: list the files you will create or change and how you will meet the PR's "Done when". Wait for approval before writing code.
- Create only the crates and folders this PR needs (blueprint §10.1).
- Before finishing, run `cargo xtask ci` (once it exists) and the PR's done-when tests. Report with the change-report format in blueprint §18.3, including the commands you ran and their results.
- Never claim a test passed unless you ran it and saw it pass.
- If something the PR needs is not decided in an ADR or the plan, stop and ask. Do not invent a decision.

## Hard rules (details in the blueprint)

- Only `SdrManager` in `crates/sdr` writes design state. Nothing else touches SDR storage.
- `schemas/` (JSON Schema) is canonical. Rust, Python and TypeScript types are generated from it; never hand-write or hand-edit them.
- Allowed crate dependencies are listed in blueprint §10.3. Do not add other edges.
- No harness logic (orchestration, scheduling, gating, SDR writes, requirement status) in Python workers or any client.
- The harness talks to tools only through MCP. No embedded interpreters, no native plugins.
- Library crates: typed errors (`thiserror`), no `unwrap`/`expect` outside tests, no `unsafe`, `tracing` for logs, timeouts on all external I/O.
- Prompts live in `prompts/`, never as string literals in Rust.
- No secrets in code, config, fixtures or logs.
- Model output is untrusted until a check validates it. Tool failures are returned as observations, never as success.

## Folders you must not change

- `docs/reference/`: inputs (old SDR notes, the four YAML schemas). Read only.
- `legacy/`: old code kept only for porting. Read only; it is deleted when no longer needed.
- `evals/benchmarks/`: benchmark problems are written by the team. Never create or edit requirement values or expected outcomes.
