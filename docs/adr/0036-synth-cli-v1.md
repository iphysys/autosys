# ADR-0036: synth CLI v1

- Status: Proposed
- Date: 2026-10-26
- Amends: ADR-0034 (the CLI surface and its parser). The API client, the event journal and the way benchmark requests are built stay as ADR-0034 decided.

## Context

`synth` (ADR-0034) has five `run` commands and prints only the API's JSON. That was enough for Increment 1's exit criteria, but daily use goes through scripts outside the product:

- `watch-run.ps1` turns `synth run watch` output into one readable line per event.
- `export-run.ps1` saves a run's report, SDR versions and events. It reads the SDR straight from Postgres with `docker exec psql`. It was written before API v0 had an SDR route; it no longer needs to.

The reference CLI (`docs/reference/codes/prime-agent-main/crates/pa-cli`) shows the shape a working CLI takes:

- command groups defined once, which drive help and typo suggestions;
- global flags;
- human output on a terminal and JSON for scripts;
- a live event view;
- export;
- a read-only `doctor`;
- confirmation before destructive commands.

API v0 already has every route this needs: the run, its events, approvals, cancel, the report, SDR versions and artifacts. So the CLI can grow without changing the server.

ADR-0034 rejected clap as "a new dependency for five commands". The surface below has fourteen commands, global flags, help text and shell completions.

## Decision

**The surface.**

```text
synth [--api-url <url>] [--output human|json] [--color auto|always|never] [--state-dir <dir>] <command>

run start   <request.json> [--idempotency-key <key>] [--watch]
run status  <run_id>
run watch   <run_id> [--from-start]
run approve <run_id> <approval_id> --answer <approve|reject> [--comment <text>]
run cancel  <run_id> [--reason <text>] [--yes]
run report  <run_id>
run export  <run_id> [--out <dir>]
sdr show    <project_id> <version>
artifact show <artifact_id>
artifact get  <artifact_id> [--out <file>]
config show
doctor
completions <shell>
```

| Command | API |
| --- | --- |
| `run status` | `GET /v0/runs/{run_id}` |
| `run export` | the report, then `GET /v0/projects/{project_id}/sdr/versions/{v}` for 0 to the final version, each artifact's metadata and content, and the events |
| `sdr show` | `GET /v0/projects/{project_id}/sdr/versions/{version}` |
| `artifact show` / `artifact get` | `GET /v0/artifacts/{artifact_id}` / `.../content` |
| `doctor` | `GET /v0/runs/{run_id}` with an id that cannot exist (below) |

The other commands keep the routes ADR-0034 gave them.

**clap (derive) parses the command line, and clap_complete writes completions.**

- Help, usage, typo suggestions and global flags all come from one definition.
- A clap usage error exits with 2, as before.
- `--help` and `--version` exit with 0.
- There is still no token flag: the token is `SYNTH_API_TOKEN` only (ADR-0022, ADR-0034).

**Output depends on where stdout goes.**

- `--output` defaults to `human` when stdout is a terminal, and to `json` otherwise.
- `json` is today's output, unchanged: the API's body, and one event per line for `watch`. Scripts, `tests/system/cli_e2e.rs` and anything piping `synth` see no change.
- `human` prints a short view:
  - one line per event, with bursts of checks grouped into one line;
  - the exact `synth run approve` command when a run asks for approval;
  - a requirement table for the report;
  - one-line receipts for `approve`, `cancel` and `start`.
- Text that comes from the model, a tool or the server has control characters removed and is cut to a maximum width before it is printed, as pa-cli does. It is untrusted.
- Colour comes through clap's own `anstream` and `anstyle`. `--color` decides, and `NO_COLOR` is honoured under `auto`. Colour never changes `json` output.

**Settings follow blueprint §11.1.** The order is defaults < environment < flags:

| Setting | Default | Environment | Flag |
| --- | --- | --- | --- |
| API URL | `http://127.0.0.1:8080` | `SYNTH_API_URL` | `--api-url` |
| State dir | `runtime-data/cli` | `SYNTH_STATE_DIR` (new) | `--state-dir` |

`--state-dir` moves from a `watch` option to a global flag, and `run watch <id> --state-dir <dir>` still parses.

**`run watch --from-start`** replays the run's events from sequence 0 into a new journal in the system temp directory, and deletes it afterwards. The run's own journal is neither read nor changed.

**`run start --watch`** starts the run, prints its id, then watches it as `run watch` would.

**`run cancel` asks first, on a terminal.**

- When stdin and stdout are both terminals and `--yes` is not given, it asks `Cancel <run_id>? [y/N]` and stops on anything but `y`.
- Without a terminal it does not ask, so scripts keep working.

**`run export`** writes to `<out>`, by default `runtime-data/outputs/<run_id>/`:

| File | Contents |
| --- | --- |
| `report.json` | the report |
| `sdr-v<N>.json` | each version from 0 to the report's final version |
| `artifacts/<artifact_id>.<format>` | the bytes of each artifact the report lists |
| `events.jsonl` | the events, through a fresh temporary journal |

- `<format>` is the artifact metadata's `format`, used only if it is 1–16 ASCII lowercase letters and digits. Otherwise the file ends in `.bin`. The id goes through `ArtifactId` parsing first. Neither can step outside `artifacts/`.
- `events.jsonl` is written only when the report is final. Before that, the stream would not end, so the command says on stderr that it skipped the file.
- The command reads only. Files it writes are written to a temporary name and renamed into place.

**`doctor` checks three things.**

1. The server answers.
2. The token is accepted. It sends `GET /v0/runs/synth-doctor-probe`. The id is valid under the `RunId` pattern, but the server only issues `run-<hex>` ids, so no run has it:
   - `404` means the token was accepted;
   - `401` means it was refused;
   - anything else is reported as it came.
3. The state dir is writable.

`doctor` reads only and never creates a run. It exits 1 if any check fails.

**`config show`** prints the URL, the state dir, the output mode and the colour mode, and whether each came from the default, the environment or a flag. It says whether `SYNTH_API_TOKEN` is set, and never prints the token.

**Unchanged from ADR-0034:**

- the exit codes: 0 on success, 2 for a usage error, 1 for anything else;
- an API error prints its `ApiError` with the correlation id on stderr; v1 adds the URL and the operation to that line;
- the idempotency key rules;
- the journal format;
- the CLI holds no logic: it builds no `RunRequest` and decides nothing a run does.

`run watch` exits 0 when the stream ends, whatever the stop reason; the stop reason is in the report.

## Alternatives considered

- **Keep the hand-written parser.** It needs no dependency, and pa-cli also parses by hand. But pa-cli needed a command table, global-flag rotation, typo suggestions and a help renderer to do it, and its help already disagrees with its parser (`send --steer` is advertised, then rejected). At fourteen commands clap is less code to keep correct.
- **Human output always, with `--json` to opt out.** That would change what every existing script and test reads.
- **A full-screen terminal UI, like pa-cli's interactive mode.** It is much larger. The line view covers following a run and answering approvals. A TUI can come later as a separate client over the same API.
- **`synth dev up|down` to start Docker, api-server and a worker.** That is developer tooling, not an API client, and `apps/cli` may not depend on `infra`. `start-dev.ps1` stays.
- **`synth run list`.** API v0 has no route for listing runs. Adding one is a schema-first server change for its own ADR.
- **A `doctor` health route on the server.** It would be cleaner, but it changes the server. A 404 for a run that cannot exist proves the token passed authentication, with the routes that already exist.

## Consequences

- New dependencies:
  - `clap` 4, with `derive` and default features;
  - `clap_complete` 4.

  Both are MIT OR Apache-2.0. `anstream` and `anstyle` come in with clap; `anstyle` is already in the tree. They are declared in `[workspace.dependencies]`, used only by `apps/cli`, and pass `cargo deny`.
- `apps/cli` still depends on `synth-api-client` alone among workspace crates, so blueprint §10.3 and arch-check are unchanged.
- `watch-run.ps1` and `export-run.ps1` are removed. Nothing outside the harness reads Postgres any more.
- A future client, such as the frontend or a TUI, can reuse nothing from `apps/cli` except what is in `api-client`. That is intended: rendering is per client.

## Links

- Blueprint §3.5, §10.3, §11.1, §13.1, §17
- ADR-0018, ADR-0022, ADR-0034
- Plan: [synth CLI v1](../plans/cli-v1.md)
- Reference: `docs/reference/codes/prime-agent-main/crates/pa-cli` (`command_registry.rs`, `daemon_command.rs`, `sessions_table_format.rs`, `daemon_discovery/stop.rs`)
