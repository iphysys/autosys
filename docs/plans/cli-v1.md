# Plan: synth CLI v1

- Status: Ready. Reviewed before any code (CLAUDE.md).
- Prepared: 2026-10-26
- Decision: [ADR-0036](../adr/0036-synth-cli-v1.md), which amends ADR-0034's CLI surface
- Builds on: ADR-0034 (`api-client`, the event journal), PR 15a (`apps/cli`), PR 15b (the SDR route)

## Why

`synth` has five commands that print raw JSON. Following a run, reading its report and saving its outputs go through `watch-run.ps1` and `export-run.ps1`. The second reads Postgres directly.

This plan grows `synth` into a CLI with the shape of prime-agent's `pa-cli`:

- command groups;
- global flags;
- human output on a terminal and JSON for scripts;
- a live event view;
- export;
- `doctor`;
- `config show`.

It needs no change to the server, the schemas or any library crate.

It is two PRs:

- **A** reshapes the CLI and adds the human view.
- **B** adds the design and artifact commands, and export.

## Out of scope

- `run list`: API v0 has no route that lists runs.
- A full-screen TUI.
- `synth dev up|down`: `start-dev.ps1` stays.
- Any change in `crates/`, `schemas/`, `config/` or `prompts/`.

## PR A: structure and human output

**Changes**

1. Add workspace dependencies in the root `Cargo.toml`, each with a comment saying why, as the others have:
   - `clap = { version = "4", features = ["derive"] }`;
   - `clap_complete = "4"`.
2. Split `apps/cli/src/main.rs` into these modules. The parser tests that exist today move into `args.rs` and keep their cases.

   | Module | What it holds |
   | --- | --- |
   | `main.rs` | tracing to stderr, the runtime, and the mapping to exit codes 0 / 1 / 2 |
   | `args.rs` | the clap derive `Cli`, the global flags, and the `run`, `config`, `doctor` and `completions` groups |
   | `settings.rs` | URL, state dir, output and colour, each with where it came from (default, environment or flag); builds `api_client::Client` (moved from `main.rs`) |
   | `commands/run.rs` | `start` (with `--watch`), `status`, `watch` (with `--from-start`), `approve`, `cancel` (with the confirmation and `--yes`), `report` |
   | `commands/config.rs` | `config show` |
   | `commands/doctor.rs` | the three checks in ADR-0036 |
   | `render/mod.rs` | `Output` (human or JSON); terminal detection with `std::io::IsTerminal`; `anstream`; `sanitize` (drop control characters, cut to a maximum width); a small table helper |
   | `render/event.rs` | the event view (below) |
   | `render/report.rs` | the run report and `run status` views |

3. **The event view** is a port of `watch-run.ps1`. `Renderer` is a pure state machine:
   - `push(&Event) -> Vec<Line>`;
   - `finish() -> Vec<Line>`;
   - a `Line` is a time, a kind, a style and text, with no I/O.

   It covers every event type in `schemas/events/event.schema.json`:
   - **`job_state_changed`:** `from -> to (reason)`.
   - **`model_call_completed`:** the model and its tokens in and out.
   - **`tool_call_started` / `tool_call_completed`:** the tool, the outcome and the duration; the error code and message; "recovered" for a recovered call.
   - **`check_completed`:** buffered. Any other event, or `finish`, flushes the buffer as one line, either "N passed (id xK, …)" or "M of N did not pass", followed by one line per check that did not pass, with its evidence id.
   - **`gate_decided`:** the gate, its decision and its reasons. The reasons are remembered.
   - **`sdr_transition`:** committed `vA -> vB`, or rejected. A rejection lists only the reasons the gate did not already print, and the remembered reasons are then cleared.
   - **`approval_requested`:** the summary, then both `synth run approve <run> <approval> --answer approve|reject` commands.
   - **`run_stopped`:** PAUSED for `approval_required`, otherwise STOPPED with the reason and the SDR version.
   - **The other types:** `run_started`, `context_assembled`, `approval_answered`, `artifact_published`, `cancel_requested` and `error` each get one line.

   The time is the event's `occurred_at` in local time, `HH:MM:SS`. All event text goes through `sanitize`.
4. `run watch` and `run start --watch` use the `Renderer` when the output is human. They still journal each event before printing it (ADR-0034).
5. **The report view** prints:
   - the stop reason and whether the report is final;
   - the final SDR version and the project;
   - a table of requirement, priority and status, with the evidence ids;
   - the artifacts, by id and format;
   - the budget used.

   The `run status` view prints the status, the project, `updated_at` and `last_seq`, and, for a suspended run, the approval it waits for.
6. Remove `watch-run.ps1`. Update the synth lines in `start-dev.ps1`'s "Ready" hints, `docs/developer-guide/running-locally.md`, `docs/developer-guide/end-to-end-example.md` if it is committed by then, and the README's CLI paragraph.
7. Add ADR-0036 to the README's ADR table.

**Tests**

- `args.rs`:
  - every command and flag parses;
  - every usage error exits 2, including a missing `--answer`, a bad `--answer`, a repeated option and an unknown command;
  - `--token` is still rejected;
  - `run watch <id> --state-dir <d>` still parses.
- `settings.rs`: the precedence order (default < environment < flag) for the URL and the state dir, and the reported source of each.
- `render/event.rs`:
  - golden text for each file in `schemas/events/examples/`, rendered with no colour and a fixed time zone;
  - a burst of checks that becomes one line;
  - a rejected `sdr_transition` that does not repeat its gate's reasons;
  - the approval hint naming the run and the approval;
  - control characters in a tool error removed.
- `render/report.rs`: golden text for `schemas/reports/examples/` (the run report).
- `render/mod.rs`: `--output` resolution for a terminal and a non-terminal, given as input, not detected inside the test.
- `commands/run.rs`: the cancel confirmation decision as a pure function of (stdin is a terminal, stdout is a terminal, `--yes`).

**Done when**

- `cargo test -p synth-cli` passes.
- `cargo xtask test-tools` passes. `tests/system/cli_e2e.rs` is unchanged: it pipes stdout, so it gets JSON.
- `cargo xtask ci` passes:
  - `cargo deny` accepts clap and clap_complete;
  - arch-check shows `synth-cli` depending on `synth-api-client` alone among workspace crates.
- `synth --help`, `synth run --help` and `synth completions powershell` work.
- A live run is checked by hand, and the result is written in the PR:
  1. `.\start-dev.ps1`;
  2. `synth run start <request> --watch` in a terminal shows the human view;
  3. the approval hint's command, run from a second window, resumes the run;
  4. `synth run report <id>` shows the table;
  5. `synth run report <id> | Out-String` gives JSON.

## PR B: design, artifacts and export

**Changes**

1. Add `commands/sdr.rs` (`sdr show <project_id> <version>`) and `commands/artifact.rs`:
   - `artifact show <id>`: the metadata;
   - `artifact get <id> [--out <file>]`: the bytes. They go to `--out` if given, otherwise to stdout, but only when stdout is not a terminal. On a terminal, `artifact get` without `--out` is a usage error, so binary data is never written to a terminal.
2. Add `commands/export.rs` (`run export <run_id> [--out <dir>]`), as ADR-0036 describes. It writes, in order:
   1. `report.json`;
   2. `sdr-v<N>.json` for 0 to the final version;
   3. `artifacts/<id>.<format>`, with the format checked against the pattern;
   4. `events.jsonl`, only if the report is final.

   Each file is written to a temporary name and renamed. In human mode, it then prints a summary: the stop reason, the requirement statuses, and the final design's parameters and decisions, as `export-run.ps1` did.
3. Human views for `sdr show` (the parameters with their values and units, and the decisions) and `artifact show`. JSON output is the API body.
4. Remove `export-run.ps1`, and update the docs that mention it.

**Tests**

- The artifact file name: an accepted format, a rejected format (`../x`, upper case, too long, empty) that gives `.bin`, and an id that does not parse.
- `artifact get` on a terminal without `--out` is a usage error.
- `tests/system/cli_e2e.rs` gets one more step after the report. `synth run export <run> --out <tmp>` is checked:
  - `report.json` equals `synth run report`;
  - `sdr-v0.json` to `sdr-v<final>.json` exist, and each parses as an SDR version;
  - `events.jsonl` has the same sequence numbers as the watch journal;
  - each artifact file's length equals its metadata's `size_bytes`.

**Done when**

- `cargo test -p synth-cli` passes.
- `cargo xtask test-tools` passes, with the new export step.
- `cargo xtask ci` passes.
- `git grep -n "docker exec\|psql"` finds nothing outside `deploy/`, `docs/` and `xtask/`.
- A live run's `synth run export` gives the same `report.json` and `sdr-v*.json` that `export-run.ps1` gave for the same run, checked before the script is deleted and recorded in the PR.

## Stop conditions: ask, don't invent

- A command needs data that API v0 does not return. Examples: the list of SDR versions for a run that has no report yet, or a run's approvals other than through its events. Do not reach past the API.
- clap or clap_complete brings a license or an advisory that `cargo deny` rejects.
- `tests/system/cli_e2e.rs` has to change for PR A. That means the JSON output changed, which ADR-0036 forbids.
- Rendering an event needs a field the event schema does not have.

## Size

- PR A: medium. Most of it is the event view and its golden tests.
- PR B: small to medium. Most of it is export and its system-test step.
