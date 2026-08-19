# horsie

## Design philosophy

**Semantic types over convenient types.** Types should encode domain intent, not just data shape. If reusing an existing type would allow a caller to pass something semantically wrong, define a new type. The name of a type is part of its contract.

**Make illegal states unrepresentable.** Use sum types (enums / tagged unions) to eliminate invalid combinations at the type level. Prefer exhaustive `match` over runtime guards — the compiler should enforce completeness, not tests.

**Deep modules.** Narrow public interface, deep implementation. A trait with two methods that hides a complex subsystem is better than a leaky abstraction that exposes internals. Every abstraction boundary should ask: what mistakes does this prevent, and what complexity does this hide?

**Compile-time over runtime enforcement.** Validate invariants at construction (builder `build()` → `Result`), not at call sites. Lints, type constraints, and the type system catch mistakes before they reach production.

**Functional / immutable by default.** Prefer append-only data, pure functions on slices, and combinator chains over mutation and shared state. Mutation should be local and obvious, never implicit.

**Protocol types are not storage types.** Wire formats and inter-module message types evolve at the speed of the interface contract. Persisted structures evolve at the speed of data migrations. Never conflate them.

## Repository layout

Every workspace crate lives under `crates/` (the workspace globs `crates/*`, so a new crate needs no `members` edit). Everything else at the root is repo-level: `clients/` (TypeScript + web UI), `docker/`, `docs/`, `scripts/`.

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

Use [fluorite](https://github.com/blossomstack/fluorite) to generate all protocol message types — any data transported between modules, or between server and clients (API request/response types, inter-crate message envelopes, wire formats).

- Define schemas as `.fl` files under `crates/models/fluorite/` (inside the models crate, so published packages are self-contained).
- The `horsie-models` crate runs `fluorite_codegen` in `build.rs` and exposes generated types via `horsie_models::models::*`.
- Generated types automatically derive `Debug`, `Clone`, `PartialEq`, `Serialize`, `Deserialize`, `JsonSchema`.
- Add hand-written convenience methods in `crates/models/src/lib.rs` (not in the schema).

**Never use fluorite for persisted data structures** (database rows, migration types, on-disk formats). Those are owned by the storage layer and must evolve independently of the wire protocol.

## Lint / fmt

Workspace lints are configured in `Cargo.toml`; each crate inherits via `[lints] workspace = true`. Production code denies `unwrap_used`, `expect_used`, `panic`, and `wildcard_enum_match_arm`. Test code opts out with `#![cfg_attr(test, allow(...))]`.

Pre-PR checks:

```bash
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```

`-D warnings` is not optional — CI adds it, so a local clippy without it exits 0 and reddens the PR.

**Never run `cargo +nightly fmt`.** `.rustfmt.toml` declares options that only exist on nightly; CI ignores them silently, and a nightly run reformats the entire tree into a diff nobody asked for. Stable `cargo fmt` only.

## Build cost, and how to iterate

`crates/server` is ~95k of the workspace's ~141k lines, and Rust's unit of recompilation is the crate. A one-line change to a widely-used module invalidates all of it, then relinks against 22 integration test binaries. Most of the wall clock of any change here is spent waiting for that, so the order you run things in matters more than it usually would.

- **Iterate with `cargo test -p horsie-server --lib <filter>`.** Nothing wider until you are ready to commit.
- **Do not alternate clippy and tests.** `cargo clippy --all-targets --all-features` and `cargo test -p horsie-server --lib` resolve *different* feature sets — `test-util` is off by default and on under `--all-features` — so they are two separate build graphs that share no artifacts. Every switch between them pays a full rebuild. Run the full clippy once, immediately before committing.
- **`-p horsie-server --lib` is a false green** for anything touching HTTP routes, the session actor's public behaviour, or recovery. Those paths are only exercised by the suites in `crates/tests`. Run the relevant one before claiming a change is done.
- **The full lib suite takes about a minute of pure execution** before any compilation. Budget for it rather than re-running it to be sure.

`sccache` is configured as the rustc wrapper, and it will report a **0% hit rate during ordinary work**. That is expected, not a misconfiguration: dependencies already built into `target/` are never handed to rustc at all, so the only compilations left are the crate you are editing, which cannot hit by definition. sccache pays off on cold builds — a fresh worktree pulling dependencies it has seen before. Do not disable incremental compilation to raise the number; incremental is what makes the inner loop fast, and sccache cannot cache it.
