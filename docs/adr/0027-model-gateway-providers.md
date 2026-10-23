# ADR-0027: Model gateway with two providers, Claude by default

- Status: Proposed
- Date: 2026-10-24

## Context

The Increment 1 plan proposed "the provider the team uses today, one adapter" behind `contracts::ModelProvider` (decision 4, PR 9). The team uses two:

- Claude, through Anthropic's Messages API;
- Pliny, an internal platform that serves other models through an OpenAI-compatible chat-completions endpoint.

Pliny does not return tool calls yet. The agents of Increment 1 work through tools, so a profile that cannot call tools must never reach them.

Current Claude models think before they answer by default (Claude Opus 5.5 always does). Within a tool-use turn, the Messages API requires the thinking blocks of the assistant turn to come back unchanged, or it rejects the request with a 400. The neutral `ContentBlock` had no place for them.

Several other questions were open when PR 9 started:

- where credentials and the internal endpoint come from;
- how an internal CA is trusted;
- what is retried;
- how cost is counted.

## Decision

**Two adapters** in `crates/model-gateway/src/providers/`, both behind `ModelProvider`, with their wire types private:

- `anthropic`: Messages API, `anthropic-version: 2026-10-21`, without streaming.
- `openai_compatible`: `POST {base}/chat/completions` with `stream: false`, used for Pliny.

**Profiles** live in `config/models/routing.yaml` (schema `schemas/config/model-routing.schema.json`). Each profile names:

- its provider and model;
- `tool_calling`;
- the names of its environment variables, never their values;
- its extra headers;
- `max_output_tokens`, a request timeout, and a retry policy.

The `prices` list holds USD per million tokens for each model.

- A profile pairs a provider with one model; the profile name is the choice of model. No model is tied to an adapter in code. The Claude options share the `anthropic` provider:
  - `haiku`: `claude-haiku-5-5`, the cheapest, and `default_profile`;
  - `sonnet`: `claude-sonnet-5-5`;
  - `opus`: `claude-opus-5-5`.

  All three run at effort `high`. Adding a model means adding a profile and its price.
- An agent's `model_profile` is optional. Without it, the agent uses the default profile.
- `SYNTH_MODEL_PROFILE` replaces the default for a run. An agent that names a profile keeps it.

**Tool-calling flag.** `ModelGateway::start` refuses a profile with `tool_calling: false` for an agent whose `tools` is not empty, with an error naming the agent and the profile. `cargo xtask validate-config` makes the same check for every agent in `config/agents/`. The Pliny profile has `tool_calling: false`.

**Startup, not first call.**

- `start` builds a provider only for the profiles in use: the default and every agent's profile.
- It reads their variables (`ANTHROPIC_API_KEY`, `PLINY_API_KEY`, `PLINY_BASE_URL`) and fails on any that is missing or blank, naming it.
- `ANTHROPIC_WORKSPACE_ID` is optional. When set, it is sent as `anthropic-workspace-id`, which the API requires for a key not scoped to one workspace.
- Keys are sent as `Authorization: Bearer`, held in a type whose `Debug` output is redacted, and marked sensitive in the HTTP client. They are never logged; neither are prompts.

**Opaque content.** `ContentBlock::Opaque { provider, data }` carries provider content the harness does not interpret, such as `thinking`, `redacted_thinking` and refusal `fallback` markers.

- The adapter whose provider id matches sends it back verbatim. Every other adapter drops it.
- Checks and the SDR never read it.
- Because it lives in the messages, it survives a worker restart.

This is a change to a core contract (blueprint §4.1 item 8, §19.3).

**TLS.**

- rustls with the ring provider, verifying against the operating system's roots through `rustls-platform-verifier`.
- `INTERNAL_CA_BUNDLE` optionally names a PEM bundle that is trusted in addition to those roots.
- No setting turns verification off.
- Base URLs must be `https`. Plain `http` is accepted only on a loopback host, for test servers.
- Redirects are not followed.

reqwest's default provider, aws-lc-rs, is not used: its OpenSSL licence is not in `deny.toml`. `webpki-root-certs` (CDLA-Permissive-2.0, root certificates as data) comes in with reqwest's rustls support and has a crate-scoped exception in `deny.toml`.

**Retries.** One bounded loop for both adapters.

- 429, 500, 502, 503, 529 and connection failures are `Unavailable`. 408, 504 and per-attempt timeouts are `Timeout`. Both codes are retried up to `max_attempts`.
- The wait is exponential backoff, or the provider's `retry-after` when it sends one, never more than `max_backoff_ms`.
- Other statuses are not retried: 400, 413 and 422 are `InvalidInput`; 401–403 are `PermissionDenied`; 404 is `NotFound`; 409 is `Conflict`.
- Each attempt and each wait ends with `Cancelled` when the `ExecutionContext` is cancelled.

**Refusal fallback.** A Claude profile may set `refusal_fallback: default`. This sends `fallbacks: "default"` with beta `server-side-fallback-2026-07-01`, so a request the safety classifiers decline is re-run server-side on the model the API picks for the refusal category. The response's `model_id` names the model that answered. `sonnet` and `opus` set it. `haiku` does not: Claude Haiku 5.5 has no server-side fallback, so a decline comes back as `Refusal`.

**Accounting.**

- Usage is normalized as in the mapping table below.
- Each call returns its usage per model, so a fallback is priced at each model's own rate.
- `cost_usd` is `None` when a model that ran has no price; an unknown cost is never reported as zero.
- A response without usage is an `Internal` error, since its call could not be budgeted.

**Mapping onto the neutral contract**

| Neutral | Anthropic | OpenAI-compatible |
| --- | --- | --- |
| `system` | top-level `system` | first `system` message |
| `ToolCall` | `tool_use` block | `tool_calls[]`, arguments as a JSON string |
| `ToolResult` | `tool_result` with `is_error` | `tool` message; an error is prefixed `Error: ` |
| `Opaque` | sent back verbatim | dropped |
| `EndTurn` | `end_turn`, `stop_sequence` | `stop` |
| `ToolUse` | `tool_use` | `tool_calls`, `function_call` |
| `MaxTokens` | `max_tokens`, `model_context_window_exceeded` | `length` |
| `Refusal` | `refusal` | `content_filter` |
| `input_tokens` | input + cache creation + cache read, summed over `usage.iterations` | `prompt_tokens` |
| `output_tokens` | `output_tokens`, summed over iterations | `completion_tokens` |

Three rules keep the contract's invariant that the stop reason is `ToolUse` exactly when the response holds a tool call:

- tool calls under an end-of-turn stop make it `ToolUse`;
- a tool-use stop without a call is an error;
- a cut-off or refused response loses its tool calls.

A tool name that is not a valid `ToolName`, or arguments that are not a JSON object, is `InvalidInput`. An unknown stop reason is `Internal`.

## Alternatives considered

- **One provider.** Leaves Pliny unusable, or Claude unused, while the team works with both.
- **Strip the thinking blocks and turn thinking off.** Not every Claude model allows it (Claude Opus 5.5 does not), and the choice would tie the default model to a gap in the contract.
- **Keep the thinking blocks in the adapter's memory.** Lost when a worker dies, which breaks recovery (PR 12).
- **An `ALLOW_INSECURE_TLS` switch, as in the team's Pliny test script.** Breaks blueprint §17; an internal CA bundle covers the same need.
- **Credentials or the Pliny endpoint in `routing.yaml`.** Secrets do not belong in configuration (blueprint §11.1, §17), and the endpoint is deployment-specific.
- **Retries inside each adapter, written twice.** Blueprint §13.1 asks for one retry mechanism, so both adapters use the same loop.
- **Streaming in v0.** Not needed at 16,000 output tokens with a 600 s timeout, and it adds an SSE parser. Kept for later.

## Consequences

- An agent with tools cannot run on Pliny until Pliny returns tool calls and its profile says `tool_calling: true`. The adapter already maps tool calls and results for that day; they are tested against hand-written chat-completions responses.
- CI uses recordings only.
  - The adapter recordings in `crates/model-gateway/tests/recordings/` are hand-written from the providers' documented wire formats, not captured.
  - `tests/live.rs` holds ignored smoke tests to run by hand with real keys.
  - `testkit::fakes::RecordedModelProvider` replays neutral exchanges for later PRs.
- With the fallback on, a run may be answered by a model other than the profile's. The response's `model_id`, which the `model_call_completed` event carries, names the model that answered, and its cost is counted at that model's price.
- Prompt caching is not used yet, so cache tokens are priced as input. Caching prices arrive with caching.
- The `model_call_completed` event has no cost field yet; PR 11, which writes the event, decides whether to add one.
- Switching models mid-conversation is safe: blocks of another provider are dropped, and the Messages API drops thinking blocks the new model cannot read.

## Links

- Blueprint §4.1 (item 8), §11, §13.1, §14.4, §17, §19.3
- `contracts::model`, `schemas/config/model-routing.schema.json`, `schemas/config/agent.schema.json`
- Increment 1 plan, decision 4 and PR 9; PRs 11 and 12 use the gateway
- Messages API: authentication and `anthropic-workspace-id`, versions, errors, thinking, refusals and fallback (platform.claude.com/docs)
