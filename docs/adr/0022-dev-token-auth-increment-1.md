# ADR-0022: Development-token authentication for Increment 1

- Status: Proposed
- Date: 2026-10-22

## Context

The blueprint requires the API to authenticate every request. The real authentication model (identity provider, sessions, per-project authorization) is Open Decision 9 and depends on the frontend, which is out of scope for Increment 1.

## Decision

- `api-server` accepts one bearer token read from an environment variable, compared in constant time.
- Requests without it, or with a wrong one, are rejected.
- The actor recorded in the audit trail is a fixed development identity.
- `api-server` refuses to start without the token set.

## Alternatives considered

- No authentication: builds a habit the frontend would have to undo.
- Full authentication now: decides Open Decision 9 before the frontend's needs are known.

## Consequences

- Not suitable for any shared or deployed environment.
- Superseded when Open Decision 9 is settled; the CLI then switches to the real flow.

## Links

- Blueprint §3.5, §17, §25 (Open Decision 9); Increment 1 plan, decision 8 and PR 14
