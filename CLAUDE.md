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

Tests must live in a separate `tests/` directory at the crate root, not inline in source files. This keeps source files focused and makes the test surface easy for LLMs/agents to read without noise.

```
my-crate/
  src/
    lib.rs
    ...
  tests/
    integration_test.rs
    common.rs   # shared helpers
```

Unit-level assertions that need access to private internals are the only exception — those stay colocated under `#[cfg(test)] mod tests` in the source file.

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
