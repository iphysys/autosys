# ADR-0023: Lease times come from the database clock

- Status: Accepted
- Date: 2026-10-23

## Context

Workers claim jobs with a lease and renew it by heartbeat (ADR-0016, blueprint §6.2). A job whose lease has expired counts as interrupted, and another worker may claim it. Deciding expiry needs one notion of "now".

Workers are separate processes, possibly on hosts whose clocks disagree. If each worker compared leases against its own clock, a worker with a fast clock could take over a job whose holder is still healthy.

The store traits in `crates/contracts` were written expecting an injected `Clock`. Testkit's in-memory stores use one, and the job-store contract suite drives it as a manual clock.

## Decision

- In the Postgres stores, lease expiry and every timestamp a store sets come from the database clock: `now()`, called through the SQL function `autosys_now()`. These stores take no `Clock`.
- `autosys_now()` exists so that tests can control the database clock in a throwaway database. Production code never redefines it.
- The in-memory stores keep their injected `Clock`, which stands in for the database clock. The contract suites run unchanged against both.

## Alternatives considered

- Each worker's own clock, injected into the Postgres stores: expiry would depend on clock skew between hosts.
- `now()` written directly in the SQL and shadowed in tests through `search_path`: works, but relies on a less obvious name-resolution rule.
- Contract suites on real time with short leases: slower tests that depend on timing.

## Consequences

- Workers agree on lease expiry, whatever their host clocks say.
- `now()` is the start time of the transaction, so every timestamp one store operation sets is the same.
- Timestamps a store sets have microsecond precision (Postgres `timestamptz`).
- The Postgres job-store contract test makes the database clock follow the suite's manual clock, through a session setting on each connection.

## Links

- ADR-0016; blueprint §6.1, §6.2
- Increment 1 plan, PR 4
- `crates/infra/migrations/0001_initial.sql`, `crates/infra/tests/contract.rs`
