# autosys

Agentic system-synthesis platform: requirements in, verified design artifacts out. A Rust harness runs an orchestrator agent that calls engineering tools (Python and Julia MCP servers in containers) and records the design in the SDR (Structured Design Representation).

The repository is built in increments. The current plan is [Increment 1: Execution Kernel](docs/plans/increment-1.md). Crates and folders appear only when the PR that needs them lands.

## Prerequisites

- [rustup](https://rustup.rs). The toolchain version is pinned in `rust-toolchain.toml`, and rustup installs it on first use. On Windows, rustup also needs the MSVC C++ build tools.
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), installed with `cargo install --locked cargo-deny`.
- [sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli) 0.9.0, installed with `cargo install --locked sqlx-cli --version 0.9.0 --no-default-features --features postgres`.
- Docker with Compose, to run PostgreSQL (see [Database](#database)).

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

Before opening a pull request, run every check CI runs:

```sh
cargo xtask ci
```

It needs Postgres running and `DATABASE_URL` set (see [Database](#database)). It runs, in order:

1. `cargo fmt --check`;
2. `cargo clippy`, with warnings as errors;
3. `cargo xtask arch-check`;
4. `cargo sqlx database setup`, which creates the database at `DATABASE_URL` if needed and applies the migrations;
5. `cargo sqlx prepare --check`, which fails if the query data in `.sqlx/` is out of date;
6. `cargo test`;
7. `cargo deny check`.

Clippy and the tests build with `SQLX_OFFLINE=true`, so they use the committed query data rather than the live database. CI starts Postgres from the same compose file and then runs only `cargo xtask ci`, so a local pass means a CI pass.

`cargo xtask arch-check` fails when a crate depends on a workspace crate that blueprint §10.3 does not allow, when `synth-testkit` is used as anything but a dev-dependency, or when a crate other than `contracts`, `testkit`, `sdr`, `infra` or `bootstrap` names `SdrRepository` (§7.2).

## Implementing a contract

The traits every harness crate implements live in `crates/contracts`. `crates/testkit` has an in-memory implementation or fake for each, and one contract-test suite per trait in `testkit::contract`. A new implementation (for example a Postgres store) runs the same suite from its own tests, with `synth-testkit` as a dev-dependency.

## Database

Jobs, events, SDR versions, evidence and artifact metadata are stored in PostgreSQL ([ADR-0016](docs/adr/0016-postgres-persistence.md)); artifact files go on a mounted volume. The stores live in `crates/infra`, and every timestamp they set, lease expiry included, comes from the database clock ([ADR-0023](docs/adr/0023-lease-times-from-database-clock.md)).

To start a local database:

1. Copy `.env.example` to `.env`, which Git ignores. Choose a password for `AUTOSYS_DB_PASSWORD` and use the same one in `DATABASE_URL`.
2. Start Postgres. It listens on 127.0.0.1 only.

   ```sh
   docker compose --env-file .env -f deploy/compose/compose.yaml up -d --wait
   ```

3. Export both variables in your shell before building or testing. Test binaries do not read `.env`.

Migrations live in `crates/infra/migrations/` and are embedded in the binary. A merged migration never changes; add a new one instead.

`sqlx` checks every query at compile time: against the database at `DATABASE_URL` when it is set, and against the committed `.sqlx/` data when `SQLX_OFFLINE=true`. After changing a query or a migration, run `cargo sqlx database setup --source crates/infra/migrations`, then `cargo sqlx prepare --workspace`, and commit `.sqlx/`.

The store tests in `crates/infra/tests/` create a database named `autosys_test_*` on the `DATABASE_URL` server for each test and drop it at the end. A test that panics leaves its database behind; drop it by hand. Without `DATABASE_URL` these tests fail; they never skip.

## Design state (SDR)

`crates/sdr` exports `SdrManager`, the only writer of design state (blueprint §7.2); its storage is crate-private. A change is proposed, validated, then committed or rejected, and the types make committing an unvalidated transition a compile error.

`validate` checks the transition against `schemas/sdr/transition.schema.json`, including constraints the Rust types do not enforce, such as `minItems`. It then requires the transition's base to be the current head, applies the operations, and checks the result against `system.schema.json`. Problems come back as typed reasons, not errors. Versions are linear: a transition on a stale base is rejected, never rebased ([ADR-0020](docs/adr/0020-sdr-v0-scope.md)).

## Schemas

`schemas/` holds the canonical contracts as JSON Schema (draft 2020-12). Rust types are generated from them at build time by `crates/schema-types`; never write or edit those types by hand. To change a type, change its schema.

- Each `*.schema.json` file has at most one root type, named by its `title`. Type names must be unique across all schema files.
- Examples live in `schemas/<area>/examples/<stem>[.<case>].json` and must validate against `<area>/<stem>.schema.json`. Examples that must be rejected go in `examples/invalid/`.
- `cargo test -p synth-schema-types` checks every schema against the 2020-12 metaschema, validates every example, and round-trips each valid example through its Rust type.
- `schema_types::SCHEMA_FILES` embeds every schema file at build time, so the harness validates values against the same schemas its types were generated from.
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
| [0023](docs/adr/0023-lease-times-from-database-clock.md) | Lease times come from the database clock | Accepted |

## License

[MIT](LICENSE)
