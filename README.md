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

This runs, in order: `cargo fmt --check`, `cargo clippy` with warnings as errors, `cargo test`, and `cargo deny check`. CI runs only `cargo xtask ci`, so a local pass means a CI pass.

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
