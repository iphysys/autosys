# ADR-0028: Pluggable model providers

- Status: Proposed
- Date: 2026-10-24
- Supersedes: the profile format of ADR-0027 (its "Profiles" decision). The rest of ADR-0027 stands.

## Context

ADR-0027 put the model gateway behind two adapters, `anthropic` and `openai_compatible`. Choosing a model is already one setting: `SYNTH_MODEL_PROFILE`, an agent's `model_profile`, or `default_profile`. Removing or adding a provider, though, touched 8–10 places, because the provider list was closed:

- `schemas/config/model-routing.schema.json` had one `oneOf` branch per provider, with that provider's fields in the profile itself;
- typify generated `ModelProfile` as an enum with one variant per provider;
- `gateway.rs` and `routing.rs` matched on that enum, and the gateway's tests used the real providers.

The team wants a provider to be one self-contained unit that can be removed or added without touching the rest of the gateway.

## Decision

**A profile has the common fields, plus `settings` owned by its provider.**

- `model-routing.schema.json`, version 0.2, fixes only what every provider shares: `name`, `provider`, `model`, `tool_calling`, `max_output_tokens`, `request_timeout_seconds`, `retry`, and `settings` (an object).
- `provider` is the name of a registered provider.
- `settings` follow the provider's own schema, `schemas/config/model-providers/<provider>.schema.json`. Rust types are generated from it as usual (`AnthropicProviderSettings`, `OpenAiCompatibleProviderSettings`); nothing is hand-written.

**Providers register.**

- `model_gateway::ProviderFactory` has four methods:
  - `name`, the `provider` value it serves;
  - `settings_schema`;
  - `check`, for what the schema cannot express;
  - `build`, which reads the profile's environment variables and returns the adapter.
- `ProviderRegistry` holds the factories in an explicit order. A name registered twice fails.
- The registry checks every profile at startup and in `cargo xtask validate-config`:
  - an unregistered provider fails, listing the registered ones (blueprint §13.1);
  - the profile's `settings` are checked against the provider's schema, and errors name the JSON path;
  - then the provider's own `check` runs.
- `providers::default_registry()` is the one list of the providers the harness ships with. Bootstrap will call it.

**One module per provider.** `crates/model-gateway/src/providers/<provider>/` holds:

- the adapter;
- the factory;
- its tests, including the checks of its profiles in the committed `routing.yaml`;
- its recordings;
- its ignored live smoke test.

Code shared by every provider stays outside the provider modules: HTTP, retries, pricing, environment reading (`EnvReader`), and header checks.

The gateway's own tests (profile resolution, the tool-calling refusal, missing variables, unregistered providers, calls) run over fake providers, so they hold whichever providers are registered.

**Removing a provider:**

1. Delete `crates/model-gateway/src/providers/<provider>/`.
2. Delete `schemas/config/model-providers/<provider>.schema.json` and its examples.
3. Remove its `pub mod` line and its entry in `default_registry()`, both in `providers/mod.rs`, and its entry in `ROOT_TYPES` in `crates/schema-types/tests/schemas.rs`.
4. Remove its profiles and prices from `config/models/routing.yaml` and its variables from `.env.example`. `validate-config` names any profile left on an unregistered provider.

Adding a provider is the reverse, plus its adapter code. Both removals were drilled in scratch worktrees following only these steps. Afterwards `cargo build`, the gateway and schema tests, `validate-config` and clippy all passed.

## Alternatives considered

- **Keep the closed `oneOf`.** Every provider change edits the schema, the enum matches and the gateway tests.
- **Cargo features per provider.** Removal would mean turning a feature off. But features must select backends only, not change core behavior (blueprint §13.1), and an unused provider's code would still sit in the tree.
- **A crate per provider.** Stronger isolation, but each crate needs an entry in the allowed-dependency table (blueprint §10.3). Not worth it at two providers; possible later, since the factory contract is the same.
- **Untyped settings with no schema.** Errors would lose their paths, and the settings would not follow the rule that `schemas/` is canonical.

## Consequences

- `routing.yaml` changes format (schema version 0.2). The committed file moves over with the same profiles and values. No other file used the 0.1 format.
- Settings errors are reported by validation against a schema at load time, not by a Rust enum. They still fail at startup and in `validate-config`, with the JSON path.
- The `ROOT_TYPES` test list still needs one line per provider schema, because each root schema must map to a generated Rust type.
- `EnvReader` is public as part of the provider toolkit, so dead-code analysis does not depend on which providers are registered.

## Links

- ADR-0027 (providers, TLS, retries, accounting, the mapping onto the neutral contract)
- ADR-0024 (validation against the canonical schemas lives in contracts)
- Blueprint §10.3, §11.2, §13.1 (registries), §14.4
- Increment 1 plan, PR 9.1
