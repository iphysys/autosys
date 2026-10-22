# ADR-0001: Record architecture decisions

- Status: Accepted
- Date: 2026-10-22

## Context

The team is rebuilding the platform and wants decisions to survive people changing, and coding agents to follow them instead of re-deciding.

## Decision

Record each architecture decision as a Markdown file in `docs/adr/`, named `NNNN-short-title.md`, with these sections: Status, Date, Context, Decision, Alternatives considered, Consequences, Links.

- Status is one of: Proposed, Accepted, Superseded by ADR-NNNN.
- An accepted ADR is not edited. A changed decision gets a new ADR that supersedes it.
- Numbers follow the ADR table in `docs/architecture/blueprint.md` §23. New decisions take the next free number.
- When an ADR is required is set by blueprint §19.3.

## Alternatives considered

- Decisions in a wiki or chat: not versioned with the code, invisible to coding agents.
- One running decisions file: hard to supersede a single decision cleanly.

## Consequences

Every PR that makes or depends on an architectural choice links its ADR. Coding agents read `docs/adr/` before work.

## Links

- Blueprint §19.3, §23
