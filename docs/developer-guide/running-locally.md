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

Start the run and watch it:

```sh
target/debug/synth run start request.json
target/debug/synth run watch <run_id>
```

`start` prints the run, including its `run_id`, and prints the idempotency key on stderr. Passing the same key again with `--idempotency-key` returns the same run instead of starting a second one.

`watch` prints one event per line as JSON. It also journals each event under `runtime-data/cli/runs/<run_id>/`, so if you stop it and run it again, it carries on after the last event it received.

Before the final design is committed, the autonomy policy asks for approval (`config/policies/autonomy.yaml`). The stream shows an `approval_requested` event with an `approval_id`, and the run waits. Answer it from another shell:

```sh
target/debug/synth run approve <run_id> <approval_id> --answer approve
```

The run continues, and `watch` ends once the run has stopped. Then read the report:

```sh
target/debug/synth run report <run_id>
```

The report gives the stop reason and, for each requirement, its status and the check findings that decided it, by evidence id. To stop a run early, use `synth run cancel <run_id> --reason "<why>"`.

## 6. Try a crash

Stop the worker with Ctrl-C mid-run, or kill it, and start it again. A stopped worker hands its job back at once. A killed one holds it until its lease expires (30 s with the committed `config/worker.yaml`). The new worker continues from the last checkpoint, and no tool call or commit happens twice ([ADR-0031](../adr/0031-scheduler-and-recovery-v0.md)). A `watch` left running sees every event once.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `SYNTH_API_TOKEN must hold the API token` | The variable is not exported in the shell running `synth` |
| `synth`: `401 permission_denied` | `synth` and `api-server` have different tokens |
| `cannot reach the API` | `api-server` is not running, or `SYNTH_API_URL` points elsewhere |
| The worker stops with a startup check about a key | `ANTHROPIC_API_KEY` is not exported in the worker's shell |
| A run fails on its first tool call | The forwarder is not up: start `build123d-forward` |

## Stopping everything

```sh
docker compose -f deploy/compose/tools.yaml down
docker compose --env-file .env -f deploy/compose/compose.yaml down
```

The database volume survives `down`. Add `-v` to the second command to drop it.
