# ADR-0025: Benchmark problem format v0

- Status: Proposed
- Date: 2026-10-23

## Context

The benchmark manifest is an extension manifest format, part of the fixed core (blueprint §4.1, item 7), so it changes only through an ADR. Blueprint §14.7 says what a problem includes: requirements, checkable expected outcomes, the required fidelity level, time and cost budgets, and the split. PR 7 encodes the first two dev problems (`docs/reference/benchmark-draft-part-design.md`). The team wrote them; PR 7 must not change, round or add any value.

The draft leaves some §14.7 items open:
- no fidelity level;
- token and cost limits "set once the model provider is chosen".

One expected outcome compares the reported mass with a closed-form formula of the final design variables.

## Decision

- **Location.** One YAML file per problem at `evals/benchmarks/<split>/<id>/problem.yaml`, validated against `schemas/benchmarks/benchmark-problem.schema.json` with the configuration loader's strict YAML parser. Only `dev` is a valid split; held-out problems never enter the repository (§9.4).
- **Contents.** A problem records:
  - `source`: the specification it encodes;
  - `status`: `placeholder` or `approved`;
  - `feasibility`: `solvable` or `infeasible`;
  - givens as quantities;
  - design variables and derived parameters, with the SDR parameter IDs they bind to;
  - requirements;
  - budgets;
  - expected outcomes;
  - optional reviewer `notes`, which are never part of an agent's task.
- **Requirements** reuse the SDR's `NumericRequirement` (ADR-0021), so a benchmark uses exactly the criteria the requirement check decides.
- **Expected outcomes.** Each keeps the specification's wording in `statement` and lists typed `checks`, decided from evidence, the SDR and the event log:
  - `stop_reason`;
  - `parameters_present`;
  - `requirements_passing`;
  - `requirement_never_passing`;
  - `parameter_matches_reference`;
  - `approval_answered`.
- **Reference values** are arithmetic expressions over the problem's parameter IDs. The grammar allows numbers, `pi`, `+ - * / ^` and parentheses, with a unit for the result and for each variable. `infra::benchmarks::expression` parses an expression into a tree and evaluates it by walking that tree; it is never executed. A new problem with a different formula needs no schema change.
- **Values the specification leaves open are left out.** There is no fidelity field yet, and budgets carry only what the specification sets. Each is added, with a schema version bump, when a specification first states it.
- **`cargo xtask validate-benchmarks`** runs in CI. Beyond the schema it checks:
  - the location against `split` and `id`;
  - unique identifiers;
  - that every reference names a declared requirement or parameter;
  - that every expression parses and uses only its declared variables;
  - that approval outcomes name a rule in `config/policies/autonomy.yaml`.

## Alternatives considered

- **A check kind for the plate's mass formula, with its constants as fields.** Simpler to evaluate, but every new formula would change a core format.
- **Outcomes as text only.** Faithful to the draft, but nothing could check them automatically. Scores must come from evidence, not judgement (§9.4).
- **A general expression language.** More than arithmetic is not needed, and anything executable has no place in evaluation assets.
- **Filling in fidelity and token budgets now.** This would add values the team did not write.

## Consequences

- PR 15's evaluation reads problems through `BenchmarkLoader` and decides each check from the run's evidence. Until then the checks are validated but not evaluated.
- Building a run's task from a problem needs values the format does not have yet. The task `Budget` requires `max_tokens` and `max_steps`, which the team sets when the model provider is chosen.
- `notes` may contain hints. Whatever builds the agent's task must leave them out.

## Links

- Blueprint §4.1, §9.4, §14.7, §16.4
- ADR-0021; Increment 1 plan, PR 7 and PR 15
- `schemas/benchmarks/benchmark-problem.schema.json`, `crates/infra/src/benchmarks/`, `evals/benchmarks/dev/`

## Amendments

### PR 15b: expressions are evaluation's

The reference expression module moves from `infra::benchmarks::expression` to `evaluation::expression`, beside the scoring that evaluates it (ADR-0035). `infra` may not depend on `evaluation` (blueprint §10.3), so:

- `BenchmarkLoader` checks the schema, identifiers and references, but no longer parses expressions;
- `evaluation::benchmarks::expression_problems` reports an expression that does not parse or uses a variable it does not declare;
- `cargo xtask validate-benchmarks` runs both, so CI checks exactly what it checked before.

Outcomes are scored by `evaluation::outcomes` as ADR-0035 describes; the format is unchanged.
