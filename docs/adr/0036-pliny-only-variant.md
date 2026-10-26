# ADR-0036: A Pliny-only variant of the harness

- Status: Proposed
- Date: 2026-10-26
- Scope: the `pliny-only` branch. `main` keeps ADR-0027's two providers.
- Amends, on this branch: ADR-0027 ("two adapters, Claude by default", the `pliny` profile's `tool_calling: false`); the entry gate of `docs/plans/pliny-tool-calling.md`

## Context

The team needs a build of the harness with no dependency on Claude models: no Claude provider, profiles, keys, fakes or fixtures. Everything the harness asks of a model is available on Pliny, the internal platform behind an OpenAI-compatible endpoint, including tool calling.

ADR-0027 recorded that Pliny did not return tool calls, so its profile had `tool_calling: false`, and agents with tools could only run on Claude. The plan for tool calling on Pliny made a captured live round trip the entry gate before the flag could change. The team now states that Pliny supports tool calling. Pliny is not reachable at the time of this change, so no round trip can be captured yet.

ADR-0028 already made providers removable, with a four-step procedure.

## Decision

**One provider.** The `anthropic` provider is removed by ADR-0028's procedure: its module, its settings schema and examples, its registry entry and `ROOT_TYPES` line, its profiles, its prices and its variables. `providers::default_registry()` holds `openai_compatible` only.

**One profile, the default.** `config/models/routing.yaml` holds `pliny` alone, as `default_profile`, with `tool_calling: true`. The startup refusal of a `tool_calling: false` profile for an agent with tools stays, tested over fake providers.

**Evidence for tool calling.** Until Pliny is reachable, CI relies on the adapter's tests against hand-written chat-completions responses, including a tool call, and on the process tests, which now drive the whole stack through a fake Pliny server. Two ignored live tests check Pliny itself: `live_pliny_tool_round_trip` in the gateway and `live_rebuilt_context_mid_turn` in the runtime. Capturing a real round trip as a recording, as the plan's steps 2–4 describe, is a follow-up once Pliny is back.

**Cost.** Pliny's model has no price, so every run reports an unknown cost, never zero (ADR-0027). `cargo xtask eval`'s `--max-cost-usd` therefore does not bind; runs are bounded by their wall-time budget, and the eval warns once per run. A price added to `prices` restores both.

**Test fixtures name no Claude model.** The fake model server is `testkit::fakes::FakePlinyServer`, which answers chat completions. Recorded model ids are the Pliny model's. Opaque blocks in fixtures carry the neutral provider id `example`; `ContentBlock::Opaque` itself is unchanged, since it is a core contract (blueprint §4.1 item 8) and other providers may need it.

## Alternatives considered

- **Keep `anthropic` registered but unused.** It leaves a Claude dependency in the code and in `cargo xtask validate-config`, which is what this variant exists to avoid.
- **Keep `tool_calling: false` until a capture exists.** Every agent of Increment 1 has tools, so the harness could not run at all.
- **Remove `ContentBlock::Opaque`.** Changes a core contract for no gain; the OpenAI-compatible adapter already drops opaque blocks.

## Consequences

- The variant runs only where `PLINY_API_KEY` and `PLINY_BASE_URL` are set and Pliny is reachable. A missing one fails at startup, naming it.
- The first live run on Pliny is also the first real test of its tool calling. If it fails, the stop conditions of `docs/plans/pliny-tool-calling.md` apply.
- The Increment 1 baseline on this branch is a Pliny baseline, run once Pliny is reachable. It is not comparable with a Claude baseline from `main`.
- Thinking blocks no longer appear in practice. The context renderer still shows opaque blocks if a future provider sends them.
- Merging `main` into this branch brings the Claude provider back; each merge repeats ADR-0028's removal steps.

## Links

- ADR-0027, ADR-0028 (providers, removal steps), ADR-0029 (context rebuilt mid-turn), ADR-0035 (evaluation and baseline)
- `docs/plans/pliny-tool-calling.md`
- Blueprint §4.1 (item 8), §13.1, §14.4
