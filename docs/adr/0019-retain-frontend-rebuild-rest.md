# ADR-0019: Retain the previous frontend; rebuild everything else

- Status: Accepted
- Date: 2026-10-22

## Context

The previous system was a single agent loop behind a web frontend and a BFF. The agent architecture is replaced, but the frontend is still useful.

## Decision

- Only the frontend code (web app and BFF) is carried over. It moves to `frontend/` when frontend integration starts, which is after the backend architecture matures.
- All backend code is rebuilt to the blueprint.
- Domain logic worth keeping (old tool code) is placed in `legacy/` as read-only reference, ported into `mcp_servers/`, then deleted.
- Data from the old system (knowledge-graph content, past SDR instances) is reviewed and migrated, not rewritten.

## Alternatives considered

- Rebuild the frontend too: discards working UI for no architectural gain.
- Keep the old backend running alongside: two sources of truth during the rebuild.

## Consequences

- Increment 1 is backend only and driven from a CLI that uses the same API the frontend will use.
- The frontend later becomes a second client of `crates/api`, through the BFF as a migration adapter (ADR-0017, open).

## Links

- Blueprint §0.1, Appendix B
