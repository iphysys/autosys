# ADR-0029: Context assembly v0

- Status: Proposed
- Date: 2026-10-24

## Context

Blueprint §8 gives `crates/context` the question "what does each model call see, and why". Its rules are:
- a context is assembled per call, not accumulated;
- assembly is deterministic, and its hash is recorded in the session log;
- compaction rebuilds from the SDR and job state;
- trusted instructions and untrusted content are labelled apart;
- every item carries its provenance.

It leaves open how large a context may be and how that is measured, what happens when it does not fit, what exactly the hash covers, how untrusted text is marked, and what a prompt header holds (§12 calls its own header "recommended"). Increment 1's query loop (PR 11) calls this crate before every model call, so these choices must be fixed first.

## Decision

**What the model sees.** Seven sections, always in this order:

| Section | Label | Holds | Cut? |
| --- | --- | --- | --- |
| `instructions` | `instructions`, the system role | `system/base.md`, then the agent's prompt | never |
| `task_contract` | `data` | goal, acceptance criteria, requirement ids, tool and skill names, budget | never |
| `job_state` | `data` | steps completed, pending work, remaining budget, SDR base version. No attempt, lease or timestamp | never |
| `design_state` | `data` | the task's SDR slice, one `<kind> <compact JSON>` line per element, kinds in a fixed order, each sorted by id | never |
| `run_history` | `data` | one structural line per relevant event: `#seq type key=value …`, no timestamps or free text | oldest first |
| `untrusted_content` | `untrusted` | the free text of those events | oldest first |
| `open_turn` | `untrusted` for tool results | the in-flight tool turn | never |

- The system prompt is the instruction bodies. `messages[0]` is one user message with a text block per section 2–6, each in `<context-section name=… trust=… source=…>` tags; SDR sections also carry `version`. The open turn follows.
- All text that did not come from `prompts/` is escaped (`&`, `<`, `>` become entities), so no input can close a section or forge a label. Attribute values also escape `"`.
- Free text of events (`error.message`, `approval_answered.comment`, `cancel_requested.reason`, and likewise gate and transition `reasons` and the approval `summary`) goes to `untrusted_content`, one `<untrusted source="event:42" field="…">` envelope each.
- Tool results are wrapped in `<untrusted source="tool:<name>" call_id="…">` envelopes by the builder; callers pass them raw.
- The model's own blocks in the open turn (text, tool calls, opaque thinking) pass through byte for byte: Anthropic requires thinking blocks back unchanged (ADR-0027). Only `ToolResult.content` is wrapped.
- An open turn starts with an assistant message and alternates. Assistant messages hold no tool results; user messages hold only tool results, each answering a call of the message before. Anything else cannot be labelled and fails with `InvalidOpenTurn` before any I/O.
- The design is read at the SDR head, or at a pinned version (`BuildRequest.at`), through `SdrManager::head` and `SdrManager::read` only. The version is recorded.
- Events: the latest `history_window_events` at or before the snapshot's last event. Not shown: `context_assembled`, `model_call_completed`, `tool_call_started` and `job_state_changed`.

**Size.** There is no tokenizer.
- A count is `ceil(UTF-8 bytes / bytes_per_token)` behind a `TokenCounter` trait. `bytes_per_token` is a profile field, starting at 3.
- `AssembledContext::compare_usage` compares the estimate with the provider's reported input tokens and logs it. The query loop calls it on every `model_call_completed`; the `context_assembled` and `model_call_completed` events stay in the log as the evidence for tuning the divisor.

**Budgets.**
- Named profiles live in `config/context.yaml` (schema `config/context-budgets.schema.json`): a total, `bytes_per_token`, the event window, an I/O timeout, and a cap per section. Only `default` exists. A call names a profile or gets `default`. The agent schema is unchanged.
- `config/policies/context.yaml` holds the hard ceiling `max_total_tokens`, 24000, equal to the default total. `cargo xtask validate-config` rejects a profile whose total exceeds it, whose five fixed caps (everything but `run_history` and `untrusted_content`) exceed its own total, a duplicate name, or a missing `default`. It also loads every prompt under `prompts/`.
- The improvement loop may rebalance caps or lower totals within the ceiling and never raise it. Raising it is a reviewed change to the policy. Blueprint §9.1 is not edited by this ADR; the edit is a follow-up.
- The effective limit is the ceiling alone. A per-model context window, with `min(ceiling, window − max_output − headroom)`, is a follow-up.

**Fitting.** Compaction is a rebuild: nothing is summarised, and the crate has no model dependency. Everything is rebuilt from its store on every call; only non-authoritative items are dropped.
1. A fixed section above its own cap fails the build with `ContextOverflow` naming it. Nothing is dropped.
2. Each trimmable section is cut to its own cap, oldest item first. The cut says so: `(N earlier items omitted, seq a-b)`, and that line counts toward the section.
3. The room left is `total − reserved tokens − fixed sections − open turn`. The open turn may exceed its own cap by borrowing from that room.
4. While the trimmable sections exceed the room, untrusted items go first, then history items, oldest first. An item is dropped whole, never cut.
5. An open turn that does not fit, or no room left even for the omission lines, fails with `ContextOverflow`.

The caps are limits, not reservations: with every fixed section at its cap only 5000 of the 24000 tokens remain.

**The hash.** sha256 of `synth-context/v1\n` followed by a length-prefixed framing of the system prompt and every block of every message, with maps as canonical JSON (sorted keys, no whitespace). The `Sha256` string has the form of the SDR's `document_sha256`.
- Hashed: exactly what the model sees.
- Not hashed: model id, tool definitions, output limit, manifest, profile name, timestamps.
- Section tags carry no event sequence number. A number that moves with every `context_assembled` would change the hash although the model sees the same content.
- Changing the framing changes the `v1` prefix.

**Recording.**
- `ContextBuilder::build` appends one `context_assembled` event after a successful assembly. The caller must not append it again. If the append fails, the build fails.
- `context_assembled` gains required `sdr_version` and `last_event_seq`: the snapshot the context was built from. A rebuild with `at` set from a recorded event gives the same hash, given the same job snapshot. This is a change to a core event (blueprint §4.1, item 1).
- `ErrorCode::ContextOverflow` (`context_overflow`, not retryable) is new in `contracts`. The builder returns it now. The adapters mapping a provider's "prompt too long" onto it is a follow-up.
- Every SDR or event-log await has the profile's I/O timeout and stops on cancellation. A timeout on the append may leave the event written, so a retry can record a second event for the same context.

**Prompts.**
- Each file in a subfolder of `prompts/` starts with a YAML header, checked against `schemas/prompts/prompt-header.schema.json`: `schema_version`, `id`, `version`, `purpose`, `inputs` (context section names; an unknown name is an error), optional `output_schema` (which must name a schema in `schemas/`), `owner`, `origin`.
- Prompts are static text with no variables. A run records each prompt's id, version and the sha256 of the file with CRLF and any BOM normalised.
- They are loaded at runtime, not compiled in, so overlays and the improvement loop can work on them later.
- In v0 `inputs` documents what a prompt expects and is not enforced.

**Tests.** The assembled context for fixed inputs is pinned in `tests/golden/context/` (blueprint §16.1), using the real `prompts/`, so a change to a prompt is a visible diff. `.gitattributes` keeps those files and the prompts at LF. `UPDATE_GOLDEN=1` rewrites them; a golden is reviewed in the diff, never accepted blindly (§16.3).

## Alternatives considered

- **Summarise history with a model.** The blueprint allows it only for non-authoritative narrative, and it would make the crate depend on a model and the hash on its output. History is already structural and small.
- **Accumulate the conversation and trim it.** Breaks "assembled per call", and recovery would depend on what a worker remembers.
- **Drop SDR elements when the context is too big.** The design is the one thing the model must not lose. An oversize slice fails loudly instead; narrowing the slice is the task's decision.
- **Trim or truncate the open turn.** A cut tool result changes what the tool said, and the provider needs the thinking blocks unchanged. It fails instead, and the tool-output size limit belongs to the tool gateway.
- **A tokenizer.** There is none for the models in use. The divisor is conservative and tunable, and the compare-usage pair measures it.
- **Ask each model for its window now.** Not recorded anywhere yet. The ceiling is a safe limit until it is.
- **Hash the manifest too.** Provenance labels are explanations of the content, not content. Hashing them would change the hash when only a label moved.
- **Prompts as constants in Rust.** Forbidden by blueprint §12.

## Consequences

- PR 11 calls `build` before every model call with `AgentSpec.prompt` (for example `orchestrator/main.md`), passes tool results raw, sets `reserved_tokens` from its tool definitions, calls `compare_usage` on every `model_call_completed`, does not append `context_assembled` itself, and decides how `ContextOverflow` becomes a stop or an escalation. It must not be handed an agent whose prompt is not in the library: `validate-config` does not check that until `config/agents/` exists.
- `RecordedModelProvider` matches requests exactly, so any change to a prompt or to the layout invalidates recordings made with it. The `v1` prefix and the goldens make such a change visible.
- After a restart, a rebuild appends another `context_assembled` with the same hash. That is expected. Replaying a context exactly needs the job as it was at that call, which the job store overwrites; PR 12 decides whether to snapshot it.
- The divisor 3 can under-count JSON-heavy content. It is unmeasured; the real window of the models in use is not in the repository. Until the adapters map a provider's overflow, one arrives as `InvalidInput` or `MaxTokens`.
- Whether Anthropic accepts a rebuilt user message in the middle of a tool-use turn that has thinking blocks is unverified. If not, PR 11 pins `at` for the whole turn.
- Events older than the window are not mentioned in the history, not even as omitted. The SDR is the record; the history is narrative.
- Multi-line values in the task contract and job state are not indented, so a goal that contains a line starting `tools:` looks like a key inside its own `data` section. The labels hold; the tool allow-list is enforced by the tool gateway, not by this text.
- The `openai_compatible` adapter drops opaque blocks but they are still counted, which over-counts and is safe.

**Follow-ups, not in this ADR's PR**
1. Model-gateway adapters map a provider's "prompt too long" to `ContextOverflow`, with recordings.
2. `context_window` per model in the routing schema, the `min()` limit above, and its check in `validate-config`.
3. Blueprint §9.1: profiles may be changed within the ceiling by the improvement loop; and §24, as noted in the Increment 1 plan.
4. An optional `context_profile` in agent specs once a second agent exists.
5. `validate-config` checks that every agent's `prompt` names a loaded prompt, when `config/agents/` exists.

## Links

- Blueprint §2.3, §4.1, §8, §9.1, §12, §16.1, §16.3
- ADR-0015, ADR-0020, ADR-0024, ADR-0026, ADR-0027, ADR-0028
- Increment 1 plan, PR 10; `crates/context`, `prompts/`, `config/context.yaml`, `config/policies/context.yaml`
