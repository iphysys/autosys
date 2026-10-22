# ADR-0002: Rust for the harness; Python and Julia for tool workers

- Status: Accepted
- Date: 2026-10-22

## Context

The previous single-loop Python harness was deprecated as a static harness. The new harness must run long, durable, multi-agent work with enforced boundaries (only the SDR manager writes design state; dependencies point inward). The engineering tools (build123d, RCAIDE, OpenCASCADE, Julia/MTK) exist only in Python and Julia.

## Decision

- The harness (orchestrator, scheduler, SDR manager, gates, context, API, CLI) is written in Rust as a Cargo workspace.
- Domain tools stay in Python or Julia, each as an MCP server in its own container.
- The only boundaries between them are MCP for calls and JSON Schema (`schemas/`) for data.

## Alternatives considered

- All Python: fastest to start, but boundaries can only be enforced by convention.
- All Rust: not possible; the CAD and analysis libraries are Python and Julia.

## Consequences

- Crate visibility and the dependency graph enforce architecture rules at compile time.
- There is no official Rust agent SDK, so the harness loop is built by the team.
- The official Rust MCP SDK (`rmcp`) is used for the tool boundary.
- Types for each language are generated from `schemas/`.

## Links

- Blueprint §3.2, §10, §13.1
