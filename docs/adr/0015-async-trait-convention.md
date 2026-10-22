# ADR-0015: Async methods on trait objects use `async-trait`

- Status: Accepted
- Date: 2026-10-22

## Context

Registries hold implementations as trait objects (`Box<dyn Tool>`, `Box<dyn ModelProvider>`, gates, checks, stores). Native `async fn` in traits is not usable through `dyn` today. Every trait in `crates/contracts` depends on this choice, so it must be fixed before PR 3.

## Decision

- Traits used as trait objects use the `async-trait` crate.
- Traits used only through generics may use native `async fn`.
- `async-trait` is declared once in `[workspace.dependencies]`.

## Alternatives considered

- Hand-written boxed futures (`Pin<Box<dyn Future + Send>>`): same cost, more boilerplate, easier to get wrong.
- Native `async fn` only: not object-safe, so registries would need enums or generics everywhere.

## Consequences

- One heap allocation per async call through a trait object. This is negligible next to model and tool latency.
- Revisit when native dynamic dispatch for async traits is stable; migrating is mechanical.

## Links

- Blueprint §13.1
