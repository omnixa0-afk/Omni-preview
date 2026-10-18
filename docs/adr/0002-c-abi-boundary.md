# ADR 0002: C ABI Boundary

## Status

Accepted

## Context

The Windows shell is written in C# and the core is written in Rust. The integration point must be explicit, stable, and easy to reason about early in the project.

## Decision

Use a stable C ABI with opaque handles and JSON payloads. Windows calls the exported functions through P/Invoke.

## Consequences

- the boundary stays simple in Stage 0
- ownership and memory management rules are explicit
- future changes can evolve the JSON schema without replacing the ABI shape
