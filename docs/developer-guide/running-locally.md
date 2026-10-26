# Running a design run locally

This guide takes you from a clean checkout to a run of the dev benchmark on your machine, driven by the `synth` CLI against a live model. It runs the same stack the end-to-end test runs: PostgreSQL and the build123d tool server in Docker, and `api-server` and `worker` with `cargo run`.

```text
synth  --HTTP-->  api-server  <-->  PostgreSQL  <-->  worker  --MCP-->  build123d (Docker)
                                                         \--HTTPS-->  Claude
```

## 1. Prerequisites

Install what the README's [Prerequisites](../../README.md#prerequisites) section lists. To run, rather than develop, you need rustup and Docker with Compose. You also need an Anthropic API key.

## 2. Environment

Copy `.env.example` to `.env`, which Git ignores, and fill in:

| Variable | Value |
| --- | --- |
| `AUTOSYS_DB_PASSWORD` | A password for the local database |
| `DATABASE_URL` | `postgres://autosys:<that password>@127.0.0.1:5432/autosys` |
| `ANTHROPIC_API_KEY` | Your key; the model profiles in `config/models/routing.yaml` name it |
| `SYNTH_API_TOKEN` | A long random string, shared by `api-server` and `synth` ([ADR-0022](../adr/0022-dev-token-auth-increment-1.md)) |

The binaries do not read `.env`, so export the variables in each shell you use:

```sh
set -a; . ./.env; set +a                     # bash or zsh
```

```powershell
Get-Content .env | Where-Object { $_ -match '^[A-Z_]+=' } | ForEach-Object {
  $name, $value = $_ -split '=', 2; Set-Item "env:$name" $value }   # PowerShell
```

Leave `SYNTH_ENVIRONMENT` unset. The committed `config/` is production configuration, and the test environment's short intervals are for tests.

## 3. Start PostgreSQL and the tool server

```sh
docker compose --env-file .env -f deploy/compose/compose.yaml up -d --wait
docker compose -f deploy/compose/tools.yaml up -d --wait build123d build123d-forward
```

The first command starts the database on 127.0.0.1:5432. The second starts the build123d server on its internal network with no internet access, plus the forwarder that publishes it on `http://127.0.0.1:18123/mcp`, the URL `config/worker.yaml` uses. See [Tool servers](../../README.md#tool-servers) for the sandbox.

## 4. Start the API server and a worker

In two shells, each with the environment exported:

```sh
cargo run -p synth-api-server -- --config config --prompts prompts
```

```sh
cargo run -p synth-worker -- --config config --prompts prompts --id worker-1
```

Both migrate the database at startup and stop with exit code 2 if a startup check fails, naming it. The API listens on `http://127.0.0.1:8080` (`config/api.yaml`). `SYNTH_MODEL_PROFILE` picks the model for the worker: `haiku` (the default), `sonnet` or `opus`.

## 5. Run the benchmark

Build the CLI once, then make the request for the dev problem:

```sh
cargo build -p synth-cli
cargo xtask benchmark-request part_design_001 > request.json
```

The request carries the problem's description and givens, its requirements and its budget. It never carries the reviewer notes or the expected outcomes ([ADR-0034](../adr/0034-cli-and-api-client.md)). Any other `RunRequest` file works the same way; see `schemas/api/run-request.schema.json`.

Check the setup, then start the run and watch it:

```sh
target/debug/synth doctor
target/debug/synth run start request.json --watch
```

`doctor` checks that the server answers, that it accepts the token and that the state dir is writable. `start` prints the run's id and prints the idempotency key on stderr. Passing the same key again with `--idempotency-key` returns the same run instead of starting a second one. With `--watch` it then follows the run, as `synth run watch <run_id>` does.

On a terminal, `watch` prints one line per event, with each burst of checks on a single line. Piped or redirected, it prints one event per line as JSON instead, and `--output human|json` overrides either. It journals each event under `runtime-data/cli/runs/<run_id>/` (`SYNTH_STATE_DIR` or `--state-dir` moves it), so if you stop it and run it again, it carries on after the last event it received. `watch --from-start` replays the whole run without touching that journal.

Before the final design is committed, the autonomy policy asks for approval (`config/policies/autonomy.yaml`). The run waits, and `watch` prints the exact commands that answer it. Run one from another shell:

```sh
target/debug/synth run approve <run_id> <approval_id> --answer approve
```

The run continues, and `watch` ends once the run has stopped. `synth run status <run_id>` shows where a run is at any time. Then read the report:

```sh
target/debug/synth run report <run_id>
```

The report gives the stop reason and, for each requirement, its status and the evidence ids of the check findings that decided it. Pipe it, or pass `--output json`, for the full JSON. To stop a run early, use `synth run cancel <run_id> --reason "<why>"`. On a terminal it asks first; `--yes` skips the question.

## 6. Try a crash

Stop the worker with Ctrl-C mid-run, or kill it, and start it again. A stopped worker hands its job back at once. A killed one holds it until its lease expires (30 s with the committed `config/worker.yaml`). The new worker continues from the last checkpoint, and no tool call or commit happens twice ([ADR-0031](../adr/0031-scheduler-and-recovery-v0.md)). A `watch` left running sees every event once.

## 7. Record the baseline

`cargo xtask eval` runs each dev benchmark problem several times against the running stack, scores every expected outcome and writes the results with their spread ([ADR-0035](../adr/0035-replay-store-and-evaluation.md)). With `api-server` and a worker running as in section 4, and `SYNTH_API_TOKEN` exported in the shell:

```sh
cargo xtask eval --repeat 5 --max-cost-usd 5
```

- Each run is driven through the API, like `synth`. The eval answers every approval with `approve`, and the baseline records that it did.
- Scores come from the run report, the event log and the SDR at every committed version, never from what the model wrote. A check the record cannot decide is `undecided`, which never counts as passing.
- No run starts once the cost reaches `--max-cost-usd`, priced from `config/models/routing.yaml`. A run past its deadline (its wall-time budget plus 5 min, or `--deadline-seconds`) is cancelled.
- The worker's `SYNTH_MODEL_PROFILE` picks the model. The results go to `evals/baselines/<date>-<profile>/baseline.json`, checked against `schemas/reports/baseline.schema.json`. Use `--out <dir>` for another directory, and `--problem <id>` to run one problem.

Every model call of a run is recorded for exact replay, whether or not the eval runs: the replay store keeps what each call sent and received, keyed by its `context_assembled` event.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `SYNTH_API_TOKEN must hold the API token` | The variable is not exported in the shell running `synth` |
| `synth`: `401 permission_denied` | `synth` and `api-server` have different tokens |
| `cannot reach the API` | `api-server` is not running, or `SYNTH_API_URL` points elsewhere |
| The worker stops with a startup check about a key | `ANTHROPIC_API_KEY` is not exported in the worker's shell |
| A run fails on its first tool call | The forwarder is not up: start `build123d-forward` |
| `eval`: `baseline.json exists` | A baseline was already recorded today for that profile: pass `--out` |

## Stopping everything

```sh
docker compose -f deploy/compose/tools.yaml down
docker compose --env-file .env -f deploy/compose/compose.yaml down
```

The database volume survives `down`. Add `-v` to the second command to drop it.
