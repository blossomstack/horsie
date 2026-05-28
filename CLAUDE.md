# october

## Coding style

### Compile-time enforcement

Prefer catching mistakes at compile time over runtime checks. Mechanisms:

- **Clippy lints** — workspace-wide via `[workspace.lints.clippy]`; production code bans panic-prone constructs:
  ```rust
  // Cargo.toml [workspace.lints.clippy]
  unwrap_used             = "deny"
  expect_used             = "deny"
  panic                   = "deny"
  wildcard_enum_match_arm = "deny"
  ```
  Test code opts out per-file:
  ```rust
  #![cfg_attr(
      test,
      allow(
          clippy::unwrap_used,
          clippy::expect_used,
          clippy::panic,
          clippy::wildcard_enum_match_arm
      )
  )]
  ```

- **Abstract data types** — use sum types (enums) to make illegal states unrepresentable. Prefer tagged enums over stringly-typed discriminators. Use the typestate/builder pattern for multi-phase initialization.

- **Functional style** — prefer immutable data structures, avoid shared mutable state, use combinator chains (`map`, `and_then`, `?`) over early returns and mutation.

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

Apply workspace lints in `Cargo.toml`:

```toml
[workspace.lints.clippy]
unwrap_used             = "deny"
expect_used             = "deny"
panic                   = "deny"
wildcard_enum_match_arm = "deny"
```

Each crate inherits via:

```toml
[lints]
workspace = true
```

Pre-PR checks:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```
