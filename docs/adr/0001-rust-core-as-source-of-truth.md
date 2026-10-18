# ADR 0001: Rust Core As Source Of Truth

## Status

Accepted

## Context

Omni is intended to grow beyond a single Windows UI shell. Core logic should not be tied to WinUI or any specific presentation layer.

## Decision

Rust is the source of truth for shared types, storage, and application behavior. The Windows app remains a thin adapter around the Rust core.

## Consequences

- future platforms can reuse the same application logic
- core behavior is tested once at the Rust layer
- C# stays focused on native shell concerns instead of domain logic
