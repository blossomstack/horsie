# october

## Design philosophy

**Semantic types over convenient types.** Types should encode domain intent, not just data shape. If reusing an existing type would allow a caller to pass something semantically wrong, define a new type. The name of a type is part of its contract.

**Make illegal states unrepresentable.** Use sum types (enums / tagged unions) to eliminate invalid combinations at the type level. Prefer exhaustive `match` over runtime guards — the compiler should enforce completeness, not tests.

**Deep modules.** Narrow public interface, deep implementation. A trait with two methods that hides a complex subsystem is better than a leaky abstraction that exposes internals. Every abstraction boundary should ask: what mistakes does this prevent, and what complexity does this hide?

**Compile-time over runtime enforcement.** Validate invariants at construction (builder `build()` → `Result`), not at call sites. Lints, type constraints, and the type system catch mistakes before they reach production.

**Functional / immutable by default.** Prefer append-only data, pure functions on slices, and combinator chains over mutation and shared state. Mutation should be local and obvious, never implicit.

**Protocol types are not storage types.** Wire formats and inter-module message types evolve at the speed of the interface contract. Persisted structures evolve at the speed of data migrations. Never conflate them.

## Tests

Unit tests live alongside source files under `#[cfg(test)] mod tests` in the same `.rs` file. E2e / integration tests that spin up the full stack go in `tests/` at the crate root.

```
my-crate/
  src/
    lib.rs        # #[cfg(test)] mod tests { ... } here
    agent.rs      # #[cfg(test)] mod tests { ... } here
  tests/
    e2e_test.rs   # full-stack integration tests only
```

## Protocol models (fluorite)

Use [fluorite](https://github.com/zhxiaogg/fluorite) to generate all protocol message types — any data transported between modules, or between server and clients (API request/response types, inter-crate message envelopes, wire formats).

- Define schemas as `.fl` files under `fluorite/` at the workspace root.
- The `models` crate runs `fluorite_codegen` in `build.rs` and exposes generated types via `models::models::*`.
- Generated types automatically derive `Debug`, `Clone`, `PartialEq`, `Serialize`, `Deserialize`, `JsonSchema`.
- Add hand-written convenience methods in `models/src/lib.rs` (not in the schema).

**Never use fluorite for persisted data structures** (database rows, migration types, on-disk formats). Those are owned by the storage layer and must evolve independently of the wire protocol.

## Lint / fmt

Workspace lints are configured in `Cargo.toml`; each crate inherits via `[lints] workspace = true`. Production code denies `unwrap_used`, `expect_used`, `panic`, and `wildcard_enum_match_arm`. Test code opts out with `#![cfg_attr(test, allow(...))]`.

Pre-PR checks:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```
