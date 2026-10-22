# autosys

Agentic system-synthesis platform: requirements in, verified design artifacts out. A Rust harness runs an orchestrator agent that calls engineering tools (Python and Julia MCP servers in containers) and records the design in the SDR (Structured Design Representation).

The repository is built in increments. The current plan is [Increment 1: Execution Kernel](docs/plans/increment-1.md). Crates and folders appear only when the PR that needs them lands.

## Prerequisites

- [rustup](https://rustup.rs). The toolchain version is pinned in `rust-toolchain.toml`, and rustup installs it on first use. On Windows, rustup also needs the MSVC C++ build tools.
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), installed with `cargo install --locked cargo-deny`.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

Before opening a pull request, run every check CI runs:

```sh
cargo xtask ci
```

This runs, in order: `cargo fmt --check`, `cargo clippy` with warnings as errors, `cargo xtask arch-check`, `cargo test`, and `cargo deny check`. CI runs only `cargo xtask ci`, so a local pass means a CI pass.

`cargo xtask arch-check` fails when a crate depends on a workspace crate that blueprint §10.3 does not allow, when `synth-testkit` is used as anything but a dev-dependency, or when a crate other than `contracts`, `testkit`, `sdr`, `infra` or `bootstrap` names `SdrRepository` (§7.2).

## Implementing a contract

The traits every harness crate implements live in `crates/contracts`. `crates/testkit` has an in-memory implementation or fake for each, and one contract-test suite per trait in `testkit::contract`. A new implementation (for example a Postgres store) runs the same suite from its own tests, with `synth-testkit` as a dev-dependency.

## Schemas

`schemas/` holds the canonical contracts as JSON Schema (draft 2020-12). Rust types are generated from them at build time by `crates/schema-types`; never write or edit those types by hand. To change a type, change its schema.

- Each `*.schema.json` file has at most one root type, named by its `title`. Type names must be unique across all schema files.
- Examples live in `schemas/<area>/examples/<stem>[.<case>].json` and must validate against `<area>/<stem>.schema.json`. Examples that must be rejected go in `examples/invalid/`.
- `cargo test -p synth-schema-types` checks every schema against the 2020-12 metaschema, validates every example, and round-trips each valid example through its Rust type.
- Units are [UCUM](https://ucum.org/) case-sensitive codes, for example `kg`, `mm`, `mm3` and `kg/m3`.

## Documentation

- [Engineering standards and repository blueprint](docs/architecture/blueprint.md) (normative)
- [Increment 1 plan](docs/plans/increment-1.md)
- [CLAUDE.md](CLAUDE.md): working rules for coding agents

### Architecture decisions

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](docs/adr/0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](docs/adr/0002-rust-harness-python-julia-tools.md) | Rust for the harness; Python and Julia for tool workers | Accepted |
| [0015](docs/adr/0015-async-trait-convention.md) | Async methods on trait objects use `async-trait` | Accepted |
| [0016](docs/adr/0016-postgres-persistence.md) | PostgreSQL for jobs, events and design state | Accepted |
| [0019](docs/adr/0019-retain-frontend-rebuild-rest.md) | Retain the frontend; rebuild the rest | Accepted |
| [0020](docs/adr/0020-sdr-v0-scope.md) | SDR v0 scope | Accepted |
| [0021](docs/adr/0021-numeric-requirement-check-v0.md) | Numeric requirement check for Increment 1 | Accepted |
| [0022](docs/adr/0022-dev-token-auth-increment-1.md) | Development-token authentication for Increment 1 | Accepted |

## License

[MIT](LICENSE)
