# ADR-0021: Numeric requirement check for Increment 1

- Status: Proposed
- Date: 2026-10-22

## Context

A run may stop with `success` only when every must-level requirement has passing evidence. Formalized requirements checked with a solver (NL-to-FOL) arrive in Increment 3, so Increment 1 needs a simpler check.

## Decision

- A deterministic check compares an SDR quantity with a requirement's threshold, using the comparators `>=`, `<=`, `=` (with tolerance) and `within` a range, after unit conversion.
- Each check writes an evidence record: inputs, values in common units, verdict.
- A requirement that cannot be expressed this way is reported `unverified`, never `pass`.

## Alternatives considered

- Let the model judge requirement satisfaction: violates the rule that models never decide what passed.
- Wait for the solver-based check: Increment 1 could never reach `success`.

## Consequences

- The check lives in `crates/evaluation` and is extended, not replaced, in Increment 3.
- Benchmark problems for Increment 1 use numeric must-level requirements.

## Links

- Blueprint §2.5, §5.4, §16.2; Increment 1 plan, decision 6 and PR 6
