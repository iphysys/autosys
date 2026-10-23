# ADR-0024: Validation against the canonical schemas lives in contracts

- Status: Accepted
- Date: 2026-10-23

## Context

`schemas/` is canonical (ADR-0003, blueprint §7.1). The generated Rust types do not enforce every schema constraint; for example `minItems`, and numbers that must be finite. Several components must therefore validate values against the schemas themselves:

- `sdr`: every proposed transition and the document it produces (PR 5);
- `evaluation`: the schema check, which records the result as evidence (PR 6);
- `infra`: configuration loading, which validates every file in `config/` (blueprint §11.2);
- `tools`: tool requests and results (blueprint §14.2, PR 8).

Blueprint §10.3 lets all of these share only `contracts` and `schema-types`. `schema-types` may depend on serialization crates only. Since PR 5, the validator has lived in `sdr`, where no other crate can reach it.

## Decision

- `contracts::schema::SchemaSet` compiles validators for chosen schema files from `schema_types::SCHEMA_FILES`. Every `$ref` resolves from those embedded files, never from the network.
- A validation result is a list of messages of the form `<instance path>: <message>`. For a tagged `oneOf` union, the messages report the errors of the branch whose tag matched.
- `contracts` depends on `jsonschema` for this. `jsonschema` is a library with no infrastructure behind it: no I/O, network, or global state.

## Alternatives considered

- A copy of the validator in each crate: about 100 lines duplicated three or four times, with error reporting that would drift apart.
- Validation in `schema-types`: breaks its "serialization crates only" rule.
- A new shared crate: needs a new row in blueprint §10.3 for one module.

## Consequences

- Every crate compiles `jsonschema`, through `contracts`.
- Changing how validation errors are reported changes them everywhere at once.

## Links

- Blueprint §7.1, §10.3, §11.2, §14.2; ADR-0020
- Increment 1 plan, PRs 5, 6 and 8
