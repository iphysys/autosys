# Plan: tool calling on Pliny

- Status: Ready, not scheduled. Start it once Pliny returns tool calls (the entry gate below).
- On the `pliny-only` branch (ADR-0036): the flag is already `true`, the refusal test is already `pliny_serves_an_agent_with_tools`, and `live_pliny_tool_round_trip` exists. What is left there is the entry gate and the captured round trip (steps 2–4), once Pliny is reachable.
- Prepared: 2026-10-24
- Increment 1 plan entry: PR 9.2
- Builds on: ADR-0027 (providers, the tool-calling flag), ADR-0028 (one module per provider)

## Why

The `pliny` profile has `tool_calling: false`, because Pliny does not return tool calls yet (ADR-0027). `ModelGateway::start` and `cargo xtask validate-config` refuse it for any agent with tools, so today Pliny serves only agents without tools.

The `openai_compatible` adapter already does the tool-calling work:

- it sends `tools`;
- it maps `tool_calls` back to `ContentBlock::ToolCall`;
- it sends tool results as `tool` messages, and marks failed ones with an `Error:` prefix;
- it maps `finish_reason`, and drops tool calls from a response that was cut off.

All of this is tested against `recordings/tool-calls.json`, which is hand-written, not captured.

ADR-0027 records the consequence: an agent with tools cannot use Pliny "until Pliny returns tool calls and its profile says `tool_calling: true`." The work is mostly evidence, plus the flag. No new ADR is needed unless a stop condition below is hit.

## Entry gate: confirm Pliny returns tool calls

Do this before opening the PR. Send one live chat-completions request to `PLINY_BASE_URL` with:

- `model: snps-provider/nemotron-3-ultra-550b-a55`;
- one tool, such as the `create_plate` tool from `test_support::request`;
- a user message that needs the tool.

Run the probe as a throwaway script outside the repository. It reads `PLINY_BASE_URL`, `PLINY_API_KEY` and `INTERNAL_CA_BUNDLE` from the environment, keeps TLS verification on, and is never committed. Then look at the response:

| Pliny returns | Meaning | Next step |
| --- | --- | --- |
| `choices[0].message.tool_calls` with `id`, `function.name` and `function.arguments` (a JSON string), and `finish_reason: "tool_calls"` | It is supported | Go ahead with this plan |
| The same, with small differences (arguments as an object, no `id`, `finish_reason: "stop"`) | It is supported, with quirks | Go ahead. Handle each quirk in step 3 |
| Tool-call-like text in `content`, for example `<TOOLCALL>[...]` | The server has no tool parser turned on | Stop. Ask the Pliny team to turn tool-call parsing on. We do not parse calls out of text |
| An error, or the tools ignored | Not supported | Stop. Wait for the Pliny team |

Send a second request that returns the tool result as a `tool` message, and confirm the model answers using it. This is the round trip.

## Changes

1. **Capture the recordings.** Store both probe exchanges in `crates/model-gateway/src/providers/openai_compatible/recordings/`, in the replay format the other recordings use:
   - add `tool-round-trip.json`, with a `description` that says "Captured from Pliny on <date>";
   - keep `tool-calls.json`: it still checks the standard OpenAI format, which other `openai_compatible` endpoints use.

   Scrub before committing:
   - no hostnames, URLs or keys;
   - no internal headers, apart from the ones `routing.yaml` already commits;
   - response ids replaced with neutral ones;
   - prompts that are only the test's own text.

   CLAUDE.md applies: no secrets in fixtures. The internal hostname is also the reason `docs/reference/model_test.py` stays uncommitted.

2. **Add replay tests** in `providers/openai_compatible/replay_tests.rs`:
   - `maps_a_captured_pliny_tool_round_trip`: replay the capture. Check the `ToolCall` name, id and arguments, the `ToolUse` stop reason, the `tool` message sent in the second request, and the usage of both calls.
   - If a quirk exists, add a unit test for it in `providers/openai_compatible/mod.rs` next to the existing mapping tests.

3. **Handle the quirks, only if the capture shows any.** Keep each change inside `providers/openai_compatible/mod.rs`. This module serves any OpenAI-compatible endpoint, so every change must also accept the standard format:
   - **arguments as an object, not a string:** accept both;
   - **a missing call id:** this stays an error (a failed check, not something we patch over). If Pliny never sends ids, stop and ask (see the stop conditions);
   - **`finish_reason: "stop"` with calls:** already handled. It maps to `ToolUse`.

4. **Turn on the flag.** In `config/models/routing.yaml`:
   - set `tool_calling: true` on `pliny`;
   - replace the comment "Pliny does not return tool calls yet…" with the date tool calling was checked;
   - change the header line `pliny … (no tool calling yet)`.

5. **Update the tests that pin the old behavior,** in `providers/openai_compatible/replay_tests.rs`:
   - replace `pliny_is_refused_at_startup_for_an_agent_with_tools` with `pliny_serves_an_agent_with_tools`, for both a pinned `model_profile: pliny` and `SYNTH_MODEL_PROFILE=pliny`;
   - leave the refusal itself alone: `gateway/tests.rs` tests it over fake providers, independent of Pliny.

6. **Add a live smoke test.** `live_pliny_tool_round_trip`, ignored like `live_pliny_text`. It is the Pliny counterpart of `live_claude_tool_round_trip`: a tool call, then a tool result, then a final answer.

7. **Docs.**
   - `README.md`, Model gateway section: Pliny supports tool calls.
   - `docs/plans/increment-1.md`: mark PR 9.2 done.
   - This file: status "Done", with the PR link.
   - ADR-0027 is not edited. Its consequence already foresaw this; the PR description cites it.

## Stop conditions: ask, don't invent

- Pliny returns calls only as text in `content`.
- Pliny omits call ids, or reuses them within a turn.
- Pliny needs request fields the adapter does not send (`tool_choice`, `parallel_tool_calls`, or a vendor field) for tools to work.
- The capture shows behavior that a change to the shared `openai_compatible` mapping would alter for other endpoints.

Each of these is a decision for an ADR, not for this PR.

## Done when

- The live round trip, `cargo test -p synth-model-gateway -- --ignored live_pliny_tool_round_trip --test-threads=1`, passes against Pliny, and the run is reported in the PR.
- The captured round trip replays in CI with no live model, and every existing gateway and provider test still passes.
- `ModelGateway::start` and `cargo xtask validate-config` accept `pliny` for an agent with tools, and still refuse any profile with `tool_calling: false` (fake-provider tests).
- Recordings hold no hostnames, keys or internal identifiers. Check with a grep for the Pliny host and for `Bearer`.
- `cargo xtask ci` passes.

## Size

- If Pliny returns standard tool calls: a small PR, about one to two hours. Most of the time goes into the capture, the scrubbing and the tests.
- With quirks: a few more hours for the step 3 mapping changes and their tests.
