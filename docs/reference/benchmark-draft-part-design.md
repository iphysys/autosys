# Draft benchmark: mounting plate (Increment 1)

> **Status: placeholder.** Drafted to unblock PR 7 and prove the kernel, not to judge design quality. The team reviews these values before the Increment 1 baseline is recorded. PR 7 encodes them exactly as written here; it does not change any value.

Two dev problems use the same part. Problem 001 is solvable; problem 002 is not, and exists to prove that the system never claims `success` when no design meets the requirements.

## The part

A flat rectangular mounting plate with four corner holes.

| Given | Value |
| --- | --- |
| Material | Aluminium 6061, density 2.70 g/cm³ (`g/cm3`) |
| Holes | 4, through, at the corners |
| Hole diameter | 6.5 mm (M6 clearance), fixed |
| Hole centre inset from each edge | 10 mm, fixed |
| Design variables | length L, width W, thickness T (all in mm) |

## Tools PR 7 must provide

Typed operations only; no tool accepts a script.

- `create_plate(length_mm, width_mm, thickness_mm, hole_diameter_mm, hole_inset_mm)` → a part handle.
- `measure(part, density)` → volume (`mm3`), mass (`g`), and bounding box length, width and height (`mm`).

## Problem 001: `part_design_001` (solvable)

All requirements are must-level.

| ID | Requirement | Quantity | Comparator | Threshold |
| --- | --- | --- | --- | --- |
| R1 | Fits the component footprint | plate length | within | 110 to 140 mm |
| R2 | Fits the component footprint | plate width | within | 70 to 90 mm |
| R3 | Stiff enough to mount | plate thickness | >= | 6 mm |
| R4 | Mass budget | plate mass | <= | 130 g |

**Why it is not trivial:** round-number footprints fail the mass budget. At 6 mm thick, 120 × 80 mm weighs 153.4 g and 120 × 70 mm weighs 133.9 g. The agent has to stay near the minimum footprint (110 × 70 mm weighs 122.6 g; 112 × 72 mm weighs 128.5 g). At 6 mm, any plate up to about 8157 mm² of area meets R4.

**Expected outcomes:**

1. Stop reason is `success`.
2. The final SDR version has L, W, T and mass as parameters with units, and R1–R4 each have passing evidence from the checks.
3. Sanity check on the tool: the reported mass is within 0.5 % of `2.70e-3 × (L × W − 4 × π × 3.25²) × T` grams for the final L, W and T in millimetres.
4. The approval before the final commit was requested and answered (plan decision 7).

## Problem 002: `part_design_002` (infeasible)

Same part and requirements as 001, except:

| ID | Requirement | Quantity | Comparator | Threshold |
| --- | --- | --- | --- | --- |
| R4 | Mass budget | plate mass | <= | 100 g |

The lightest plate allowed by R1–R3 (110 × 70 × 6 mm) weighs 122.6 g, so no design meets all four.

**Expected outcomes:**

1. Stop reason is `failure` or `budget_exhausted`, never `success`.
2. No SDR version is committed with R4 shown as passing.

## Budgets (both problems)

| Budget | Limit |
| --- | --- |
| Tool calls | 30 |
| Wall time | 10 min |
| Repair attempts per step | 3 |

Token and cost limits are set once the model provider is chosen.
