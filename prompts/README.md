# Prompts

Prompts are versioned assets of the harness (blueprint section 12). They are loaded at runtime by `PromptLibrary::load_dir("prompts/")` in `crates/context`. They are never string literals in Rust.

**The wording is for the team to review.** Every prompt is part of the golden tests, so a change to a prompt shows up in the diff of `tests/golden/context/`.

## Layout

```text
prompts/
  system/base.md          instructions every context starts with
  orchestrator/main.md    the orchestrator's own prompt
```

- Only files in a subfolder are prompts. Files directly in `prompts/`, such as this one, are documentation.
- A prompt's path is its name: an agent spec names `orchestrator/main.md` in its `prompt` field. Paths use lowercase letters, digits, `_` and `-`, in folders, ending in `.md`.
- The system prompt of a model call is `system/base.md`, then the agent's prompt.

## Header

Every prompt starts with a YAML header between `---` lines, checked against `schemas/prompts/prompt-header.schema.json`. `cargo xtask validate-config` checks every header.

| Field | |
| --- | --- |
| `schema_version` | `"0.1"` |
| `id` | Unique across `prompts/`, for example `orchestrator.main` |
| `version` | SemVer. Raise it when the wording changes |
| `purpose` | One clear purpose |
| `inputs` | The context sections the prompt expects: any of `instructions`, `task_contract`, `job_state`, `design_state`, `run_history`, `untrusted_content`, `open_turn`. A name that is not a section is an error |
| `output_schema` | Optional. A schema in `schemas/` the model's output must satisfy |
| `owner` | Who maintains it |
| `origin` | `human` or `improvement-loop` |

In v0 `inputs` documents what a prompt expects; the builder does not enforce it.

A run records each prompt's id, version and sha256. The hash is taken over the file with CRLF turned into LF and any byte-order mark removed, so a checkout on Windows gives the same hash.

## Static text

A prompt is static. It has no variables and no includes. Everything that changes from call to call, such as the task, the job, the design and the events, goes into its own section of the context, escaped and labelled. A prompt never holds a number, a name or a value taken from a run.

## Trust labels

| Label | Where | Holds |
| --- | --- | --- |
| `instructions` | the system prompt | these prompts, and nothing else |
| `data` | `<context-section trust="data">` | records the harness built: task contract, job state, design state, run history |
| `untrusted` | `<context-section trust="untrusted">`, `<untrusted>` | free text and tool output |

All text that did not come from `prompts/` is escaped (`&`, `<`, `>`), so no input can close a block or forge a label. `system/base.md` tells the model what the labels mean.

## Rules for prompt text

- A prompt grants no permissions, bypasses no validation, and is never the only enforcement of a rule. Gates and schemas enforce; a prompt only asks.
- No secrets, credentials or deployment values.
- A prompt names no tool. Which tools exist is in the task contract.

## Changing a prompt

1. Edit the file and raise `version`.
2. Run `UPDATE_GOLDEN=1 cargo test -p synth-context --test golden`.
3. Read the diff of `tests/golden/context/`. Every changed line is a change in what the model sees. Accept it only if it is what you meant.
4. Commit the prompt and the goldens together.

Changes from the improvement loop need an improvement report (blueprint section 9).
