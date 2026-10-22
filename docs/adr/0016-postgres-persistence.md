# ADR-0016: PostgreSQL for jobs, events and design state

- Status: Proposed
- Date: 2026-10-22

## Context

`api-server` and `worker` are separate processes. Increment 1 needs:

- durable jobs that a worker claims with a lease and renews by heartbeat;
- an append-only event log with gap-free per-run sequence numbers, so clients can resume a stream;
- new events pushed to waiting streams and waiting jobs;
- versioned SDR state, evidence, approvals and artifact metadata.

## Decision

- PostgreSQL, accessed through `sqlx`, with migrations in the repository.
- Jobs are claimed with `SELECT … FOR UPDATE SKIP LOCKED`.
- `LISTEN/NOTIFY` signals new events and wake-ups.
- Artifact files go on a mounted volume; only their metadata is in Postgres.
- Every store is behind a trait in `crates/contracts`, so the backend stays swappable.

## Alternatives considered

- SQLite: simple, but two writer processes contend, and it has no notification mechanism.
- A message queue plus a separate database: more moving parts than Increment 1 needs.

## Consequences

- Local development needs Postgres, provided by `deploy/compose/`.
- `sqlx` compile-time query checks need a database or offline query data; CI uses offline mode with the query data committed.
- Each store has a testkit contract suite that runs against both the in-memory and Postgres implementations.

## Links

- Blueprint §6, §3.5; Increment 1 plan, decision 1 and PR 4
