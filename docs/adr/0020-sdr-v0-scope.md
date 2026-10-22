# ADR-0020: SDR v0 scope

- Status: Accepted
- Date: 2026-10-22

## Context

Increment 1 needs a design record that checks can evaluate, while the full design workflow (Open Decision 1) is still being settled. `docs/reference/SDR_old.md` describes the previous SDR, which already had decision rationale and dependency invalidation.

## Decision

SDR v0 contains:

- **Requirements:** ID, text, priority (`must` or `should`), and where numeric: quantity, comparator, threshold and unit.
- **Parameters:** name, value, unit, source (tool result, assumption or user).
- **Decisions:** what was chosen, rationale, and the requirements or parameters it relates to.
- **Assumptions:** recorded as elements so they can be invalidated later.
- **Evidence links:** from requirements and parameters to evidence records.

Versions are linear (no branches). One committed transition creates one version. The JSON Schema in `schemas/sdr/` is canonical.

## Alternatives considered

- Port the old SDR as-is: carries its known gaps (no transactions, units as labels).
- Model full SysML v2 concepts now: too much before the design workflow is agreed.

## Consequences

- Increment 2 adds branches, dependency invalidation and migrations on top of v0.
- Concepts from `SDR_old.md` not listed here are deferred, not dropped.

## Links

- Blueprint §7; Increment 1 plan, decision 5 and PRs 2 and 5; `docs/reference/SDR_old.md`
