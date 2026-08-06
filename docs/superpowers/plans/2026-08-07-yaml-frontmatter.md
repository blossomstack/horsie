# YAML Frontmatter Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Parse Claude-compatible YAML frontmatter without dropping skills that contain multiline fields such as `allowed-tools`.

**Architecture:** Preserve the existing fence splitter and consumer-facing helper functions, but parse the fenced header with a Serde-compatible YAML parser. Deserialize into a generic YAML mapping so malformed YAML still fails while unknown structured provider fields are ignored by existing consumers.

**Tech Stack:** Rust, Serde, YAML parser, Cargo unit tests.

## Global Constraints

- Keep the public frontmatter helper behavior and existing consumer APIs unchanged.
- Ignore unknown frontmatter fields rather than requiring Horsie to model every provider field.
- Preserve best-effort parsing: malformed frontmatter returns `None` to current callers.
- Add regression coverage before implementation.

---

### Task 1: Add dependency and regression coverage

**Files:**
- Modify: `support/Cargo.toml` — add the YAML parser dependency.
- Modify: `support/src/frontmatter.rs` — add tests proving multiline and structured YAML currently fail.

**Interfaces:**
- Consumes: existing `split()` and `pairs()` helpers.
- Produces: failing tests that define accepted Claude-compatible frontmatter.

- [ ] **Step 1: Add the YAML dependency declaration**

Add the chosen Serde-compatible YAML crate to the support crate's regular dependencies, matching the workspace dependency style.

- [ ] **Step 2: Write the multiline-list regression test**

Add a unit test that passes:

```yaml
name: impeccable
description: Design fluency
allowed-tools:
  - Bash(npx impeccable *)
  - Bash(node scripts/*)
```

and asserts `pairs()` returns `Some` containing `name` and `description`.

- [ ] **Step 3: Write the unknown-structured-field regression test**

Add a test with a nested or list-valued unknown field and assert the known scalar fields remain available.

- [ ] **Step 4: Run the focused tests and verify the new tests fail**

Run:

```bash
cargo test -p horsie-support frontmatter
```

Expected: existing tests pass, and the new structured-frontmatter tests fail against the flat parser.

### Task 2: Replace flat parsing with YAML deserialization

**Files:**
- Modify: `support/src/frontmatter.rs` — parse the header through YAML while preserving the existing return shape.

**Interfaces:**
- Consumes: `frontmatter: &str`.
- Produces: `Option<Vec<(&str, &str)>>` or an equivalent internal representation that existing callers can use without API changes.

- [ ] **Step 1: Deserialize YAML into a generic mapping**

Use the YAML crate to validate and parse the header. Convert scalar key/value pairs into the existing borrowed string pairs; ignore non-scalar values and unknown fields. Ensure the returned values remain valid for the input lifetime or adjust only private helper signatures as needed.

- [ ] **Step 2: Preserve malformed-input behavior**

Return `None` when YAML cannot be parsed, while continuing to accept the existing flat scalar tests, quoted values, comments, and CRLF fence handling.

- [ ] **Step 3: Run focused tests**

Run:

```bash
cargo test -p horsie-support frontmatter
```

Expected: all frontmatter tests pass, including multiline Impeccable-style metadata.

### Task 3: Format and verify repository behavior

**Files:**
- Modify only files required by Tasks 1–2.

- [ ] **Step 1: Format the changed Rust files**

Run:

```bash
cargo fmt --all
```

- [ ] **Step 2: Run focused support tests**

Run:

```bash
cargo test -p horsie-support
```

Expected: pass.

- [ ] **Step 3: Run workspace tests and lint checks**

Run:

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected: all commands pass.

- [ ] **Step 4: Review the diff and commit implementation**

Run:

```bash
git diff --check
git status --short
git add support/Cargo.toml support/src/frontmatter.rs Cargo.lock
git commit -m "fix: parse plugin frontmatter as YAML"
```

### Task 4: Open the pull request

- [ ] **Step 1: Push the branch**

```bash
git push -u origin fix/yaml-frontmatter
```

- [ ] **Step 2: Open a conventional PR**

Use title:

```text
fix: parse plugin frontmatter as YAML
```

Explain that the handwritten flat parser rejected valid multiline Claude metadata and that YAML parsing now preserves known scalar fields while ignoring unknown structured fields. Include focused and workspace verification results.

- [ ] **Step 3: Check PR CI status**

Wait for CI and report any failures honestly; fix failures before considering the work complete.
