# Sandbox-by-default for `horsie connect` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `horsie connect` sandbox its runtimes by default (opt out via `--no-sandbox`, fail-closed startup probe), and give the session server a real default capability spec so default-on actually works.

**Architecture:** The server authors per-session capability specs; its placeholder `default_caps` (block network, zero grants) becomes a real cross-platform default (union of the retired CLI Linux/macOS defaults). `horsie connect` flips to sandbox-on, probes nono support at startup via the runtime's existing `probe` subcommand, and refuses to start unless `--no-sandbox` is passed. The `RuntimeVendor` library default stays off; plugin-library grants are already merged by `write_caps_file`.

**Tech Stack:** Rust 2024, clap 4, serde/serde_json, fluorite-generated `horsie_models::capabilities`, nono (Landlock/Seatbelt).

**Spec:** `docs/superpowers/specs/2026-08-02-connect-sandbox-default-design.md`

## Global Constraints

- Workspace lints deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in production code; test modules opt out with the `#[cfg_attr(test, allow(...))]` header already used in each file.
- Unit tests live in the same `.rs` file under `#[cfg(test)] mod tests`.
- Pre-PR checks: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`.
- fmt with the stable toolchain (never nightly).
- Commit after each task.

---

### Task 1: Server default capability spec

**Files:**
- Create: `server/src/default_capabilities.json`
- Create: `server/src/default_capabilities.rs`
- Modify: `server/src/lib.rs` (register module, alphabetically after `pub mod config;`)
- Modify: `server/src/bin/horsie-server/main.rs:79-90` (replace the placeholder `default_caps`)
- Delete: `cli/src/capabilities/default.linux.json`, `cli/src/capabilities/default.macos.json` (dead since #114; content moves to the server)
- Modify: `runtime/Cargo.toml:39` (stale comment pointer `cli/src/capabilities.rs` → `server/src/default_capabilities.rs`)

**Interfaces:**
- Consumes: `horsie_models::capabilities::{CapabilitySpec, Grant, Access, NetworkPolicy, BlockNetwork, WorkingDirGrant}` (all existing).
- Produces: `horsie_server::default_capabilities::default_capabilities() -> Result<CapabilitySpec, String>` — used by the `horsie-server` binary in this task.

- [ ] **Step 1: Write the failing test module**

Create `server/src/default_capabilities.rs` with only the tests first (the function does not exist yet — compile failure is the red):

```rust
//! The default capability spec the server hands a session whose creation
//! request supplied none. `horsie connect` sandboxes its runtimes by default,
//! so this spec is enforced: it must confine a runtime without breaking it —
//! the working dir read-write, the system toolchain read-only, network
//! blocked. One spec serves every vendor OS: the runtime skips `Dir`/`File`
//! grants whose paths are absent on the host and ignores Seatbelt rules off
//! macOS, so the union of the per-OS defaults is safe everywhere.

use horsie_models::capabilities::CapabilitySpec;

const DEFAULT_CAPABILITIES_JSON: &str = include_str!("default_capabilities.json");

/// The built-in default spec, parsed from the embedded JSON. Returns `Err`
/// instead of panicking because workspace lints deny `expect` in production
/// code; a corrupt embedded file fails server startup, loudly.
pub fn default_capabilities() -> Result<CapabilitySpec, String> {
    serde_json::from_str(DEFAULT_CAPABILITIES_JSON)
        .map_err(|e| format!("built-in default capability spec parse error: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::capabilities::{Access, Grant, NetworkPolicy, WorkingDirGrant};

    #[test]
    fn default_spec_parses() {
        default_capabilities().expect("embedded default spec must parse");
    }

    #[test]
    fn default_spec_blocks_network_and_grants_working_dir_read_write() {
        let spec = default_capabilities().unwrap();
        assert!(
            matches!(spec.network, NetworkPolicy::Block(_)),
            "default must block network egress"
        );
        assert!(
            spec.grants.contains(&Grant::WorkingDir(WorkingDirGrant {
                access: Access::ReadWrite,
            })),
            "default must grant the working dir read-write"
        );
    }

    #[test]
    fn default_spec_allows_macos_security_server_for_tls() {
        let spec = default_capabilities().unwrap();
        let rules = spec.unsafe_seatbelt_rules.unwrap_or_default();
        assert!(
            rules.iter()
                .any(|r| r == r#"(allow mach-lookup (global-name "com.apple.SecurityServer"))"#),
            "macOS Secure Transport needs SecurityServer to validate TLS certs"
        );
    }

    #[test]
    fn default_spec_grants_system_toolchain_reads() {
        let spec = default_capabilities().unwrap();
        for path in ["/usr", "/bin", "/etc"] {
            let present = spec.grants.iter().any(
                |g| matches!(g, Grant::Dir(d) if d.path == path && d.access == Access::Read),
            );
            assert!(present, "default missing read dir grant for {path}");
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server default_capabilities`
Expected: FAIL to compile — `default_capabilities.json` does not exist yet (`include_str!` error).

- [ ] **Step 3: Create the embedded default spec JSON**

Create `server/src/default_capabilities.json` — the union of the retired `cli/src/capabilities/default.{linux,macos}.json` grant lists, plus the Seatbelt rule the old CLI injected via `with_default_seatbelt_rules`:

```json
{
  "network": { "type": "Block", "value": {} },
  "grants": [
    { "type": "Dir", "value": { "path": "/usr", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/bin", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/sbin", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/lib", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/lib64", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/etc", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/opt", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/proc", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/System", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/Library", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/private/etc", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/var", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/private/var", "access": "Read" } },
    { "type": "Dir", "value": { "path": "/dev/fd", "access": "Read" } },
    { "type": "WorkingDir", "value": { "access": "ReadWrite" } },
    { "type": "File", "value": { "path": "/dev/null", "access": "ReadWrite" } },
    { "type": "File", "value": { "path": "/dev/zero", "access": "Read" } },
    { "type": "File", "value": { "path": "/dev/urandom", "access": "Read" } },
    { "type": "File", "value": { "path": "/dev/random", "access": "Read" } },
    { "type": "File", "value": { "path": "/dev/tty", "access": "ReadWrite" } }
  ],
  "unsafeSeatbeltRules": [
    "(allow mach-lookup (global-name \"com.apple.SecurityServer\"))"
  ]
}
```

- [ ] **Step 4: Register the module and run tests**

In `server/src/lib.rs`, insert after `pub mod config;`:

```rust
pub mod default_capabilities;
```

Run: `cargo test -p horsie-server default_capabilities`
Expected: PASS (4 tests).

- [ ] **Step 5: Use it in the `horsie-server` binary**

In `server/src/bin/horsie-server/main.rs`, replace the comment block and `default_caps` construction (currently lines 79-90):

```rust
    // No vendor enforces the per-session capability spec today (the old
    // server-spawned sandboxed local vendor was replaced by the user-launched
    // LocalDaemonVendor in #8) — supply a fixed minimal default and pass any
    // request-supplied spec through unchanged. Matches `AppState`'s own doc
    // comment ("injected by the host binary, which owns the capability-
    // resolution helpers") and the fallback the crate's own tests already use.
    let caps_finalize: CapsFinalize = Arc::new(|caps| caps);
    let default_caps = CapabilitySpec {
        network: NetworkPolicy::Block(BlockNetwork {}),
        grants: vec![],
        unsafe_seatbelt_rules: None,
    };
```

with:

```rust
    // `horsie connect` sandboxes its runtimes by default, so this default is
    // enforced on every session that didn't supply its own spec: it must
    // confine a runtime without breaking it (working dir read-write, system
    // toolchain reads, network blocked). A request-supplied spec passes
    // through `caps_finalize` unchanged.
    let caps_finalize: CapsFinalize = Arc::new(|caps| caps);
    let default_caps = horsie_server::default_capabilities::default_capabilities()
        .map_err(BootError::Config)?;
```

Remove the now-unused import line 22 (`use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};`) if nothing else in the file uses those names — check first with `grep -n "CapabilitySpec\|BlockNetwork\|NetworkPolicy" server/src/bin/horsie-server/main.rs`.

- [ ] **Step 6: Delete the dead CLI capability files and fix the stale pointer**

```bash
git rm cli/src/capabilities/default.linux.json cli/src/capabilities/default.macos.json
```

In `runtime/Cargo.toml` line 39, change:

```
# Upstream nono. The macOS SecurityServer TLS allow is injected as a capability rule
# (see cli/src/capabilities.rs), not a fork. default-features=false drops system-keyring.
```

to:

```
# Upstream nono. The macOS SecurityServer TLS allow is injected as a capability rule
# (see server/src/default_capabilities.json), not a fork. default-features=false drops system-keyring.
```

- [ ] **Step 7: Verify and commit**

Run: `cargo test -p horsie-server` and `cargo build -p horsie-server --bin horsie-server`
Expected: PASS / clean build.

```bash
git add server/src/default_capabilities.rs server/src/default_capabilities.json server/src/lib.rs server/src/bin/horsie-server/main.rs runtime/Cargo.toml
git commit -m "feat(server): real default capability spec for sessions that supply none"
```

---

### Task 2: `horsie connect` — `--no-sandbox` and the startup probe

**Files:**
- Modify: `cli/src/main.rs` (Connect variant flag ~lines 60-63; match arm ~lines 302-336)
- Modify: `cli/src/connect.rs` (probe functions + call in `run`; unit tests)

**Interfaces:**
- Consumes: `horsie_models::capabilities::{Access, BlockNetwork, CapabilitySpec, Grant, NetworkPolicy, WorkingDirGrant}` (`horsie-models` is already a cli dependency); the runtime's `probe` subcommand contract (`horsie-runtime probe --workspace probe=<dir> --sandbox-caps <file>`, exit 0 = applied).
- Produces: `connect::run` keeps its exact signature `(runtime_bin, server, workspaces, vendor_name, background, state_dir, plugins, sandbox: bool) -> Result<i32, CliError>`; the caller now passes `!no_sandbox`.

- [ ] **Step 1: Write the failing tests**

In `cli/src/connect.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn probe_verdict_accepts_only_exit_zero() {
        assert!(probe_verdict(Some(0)).is_ok());
        for exit in [Some(3), Some(2), Some(1), None] {
            let err = probe_verdict(exit).expect_err("only exit 0 proves confinement");
            assert!(format!("{err}").contains("--no-sandbox"), "{err}");
        }
    }
```

Run: `cargo test -p horsie --lib connect::tests::probe_verdict`
Expected: FAIL to compile — `probe_verdict` does not exist.

- [ ] **Step 2: Implement the probe in `cli/src/connect.rs`**

Add to the imports at the top of the file:

```rust
use horsie_models::capabilities::{
    Access, BlockNetwork, CapabilitySpec, Grant, NetworkPolicy, WorkingDirGrant,
};
```

Add these functions before `run`:

```rust
/// Exit-status classification of a `horsie-runtime probe` run: 0 proves the
/// sandbox applied on this host; anything else (3 = unsupported, other codes,
/// a signal) cannot prove confinement, so startup is refused.
fn probe_verdict(exit: Option<i32>) -> Result<(), CliError> {
    match exit {
        Some(0) => Ok(()),
        Some(_) | None => Err(CliError::Validation(
            "the nono sandbox is not supported on this host; re-run with \
             `--no-sandbox` to spawn unsandboxed runtimes"
                .to_string(),
        )),
    }
}

/// Prove the sandbox works on this host before serving sessions: run the
/// runtime's own `probe` subcommand against a minimal spec (the state dir as
/// the working dir, network blocked). The probed binary is the same one the
/// agent spawns per session, so this exercises the production path in
/// milliseconds — no endpoint, no connect-retry budget. It proves nono can
/// apply a spec here; it does not pre-validate the specs the server will send.
fn probe_sandbox_support(runtime_bin: &Path, state_dir: &Path) -> Result<(), CliError> {
    let spec = CapabilitySpec {
        network: NetworkPolicy::Block(BlockNetwork {}),
        grants: vec![Grant::WorkingDir(WorkingDirGrant {
            access: Access::ReadWrite,
        })],
        unsafe_seatbelt_rules: None,
    };
    let caps = state_dir.join("probe-capabilities.json");
    let bytes = serde_json::to_vec(&spec)
        .map_err(|e| CliError::Validation(format!("encode probe capability spec: {e}")))?;
    std::fs::write(&caps, bytes).map_err(|e| CliError::Io(e.to_string()))?;
    let status = std::process::Command::new(runtime_bin)
        .arg("probe")
        .arg("--workspace")
        .arg(format!("probe={}", state_dir.display()))
        .arg("--sandbox-caps")
        .arg(&caps)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| CliError::Executor(format!("spawn sandbox probe: {e}")))?;
    probe_verdict(status.code())
}
```

In `run`, immediately after `std::fs::create_dir_all(state_dir)...` and before the socket setup, add:

```rust
    if sandbox {
        probe_sandbox_support(runtime_bin, state_dir)?;
    }
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p horsie --lib connect`
Expected: PASS (existing connect tests + the new verdict test).

- [ ] **Step 4: Swap the CLI flag in `cli/src/main.rs`**

In the `Connect` variant, replace:

```rust
        /// Apply the server's sandbox policy to every runtime this agent
        /// spawns. Off by default: the machine is already your own.
        #[arg(long)]
        sandbox: bool,
```

with:

```rust
        /// Do not sandbox the runtimes this agent spawns: the server's
        /// capability spec is ignored and runtimes inherit the ambient
        /// environment. Sandboxing is on by default.
        #[arg(long)]
        no_sandbox: bool,
```

In the `Command::Connect` match arm, rename the binding `sandbox,` to `no_sandbox,` and change the last argument of the `connect::run(...)` call from `sandbox,` to `!no_sandbox,`.

- [ ] **Step 5: Verify the flag surface**

Run: `cargo build -p horsie && ./target/debug/horsie connect --help`
Expected: help shows `--no-sandbox` and no `--sandbox`; build clean.

- [ ] **Step 6: Commit**

```bash
git add cli/src/main.rs cli/src/connect.rs
git commit -m "feat(cli): sandbox horsie connect runtimes by default with a fail-closed startup probe"
```

---

### Task 3: E2E flag, doc comments, guide updates

**Files:**
- Modify: `cli/tests/connect_e2e.rs` (two long-lived agent spawns, ~lines 118-129 and ~308-319)
- Modify: `runtime-vendor/src/vendor.rs` (field comment ~lines 128-131)
- Modify: `docs/guide/getting-started.md:48`
- Modify: `docs/guide/runtime-vendors.md:48` (read surrounding lines first — confirm exact wording before editing)

**Interfaces:**
- Consumes: the `--no-sandbox` flag from Task 2.
- Produces: nothing code-facing.

- [ ] **Step 1: Pass `--no-sandbox` in the connect e2e**

In `cli/tests/connect_e2e.rs`, add `"--no-sandbox",` to the `.args([...])` list of the two tests that spawn a long-lived `horsie connect` (the vendor-chain test and `runtimes_die_with_the_agent`). These tests exercise the vendor chain with `sandbox_capabilities: None` and must not become host-dependent on nono support. The `--background` refusal test exits before the probe and is left unchanged.

- [ ] **Step 2: Update the `RuntimeVendor::sandbox` field comment**

In `runtime-vendor/src/vendor.rs`, replace:

```rust
    /// Whether to honor the server's sandbox spec. Off by default so the local
    /// vendor keeps behaving as it does today, where the machine is already the
    /// user's own; `horsie connect --sandbox` turns it on.
    sandbox: bool,
```

with:

```rust
    /// Whether to honor the server's sandbox spec. The library default is off;
    /// `horsie connect` turns it on unless started with `--no-sandbox`.
    sandbox: bool,
```

- [ ] **Step 3: Update the guide docs**

In `docs/guide/getting-started.md` line 48, replace the sentence:

```
Add `--sandbox` to apply the server's sandbox policy to each runtime.
```

with:

```
Runtimes are sandboxed by default with the server's capability spec (the agent probes sandbox support at startup and refuses to start on a host that can't be confined); pass `--no-sandbox` to run unsandboxed.
```

In `docs/guide/runtime-vendors.md` around line 48, replace the `--sandbox` bullet (read it first — it currently says the flag is off by default) with a `--no-sandbox` bullet describing the default-on behavior.

- [ ] **Step 4: Verify and commit**

Run: `cargo test -p horsie --test connect_e2e`
Expected: PASS (requires the workspace built; tests skip gracefully if `horsie-runtime` is absent).

```bash
git add cli/tests/connect_e2e.rs runtime-vendor/src/vendor.rs docs/guide/getting-started.md docs/guide/runtime-vendors.md
git commit -m "docs+test: connect e2e opts out of the sandbox; guide and vendor docs describe default-on"
```

---

### Task 4: Pre-PR verification and PR

- [ ] **Step 1: Full verification**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```

Expected: all green. Fix anything that isn't before proceeding.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin connect-sandbox-default
gh pr create --title "feat(cli): sandbox horsie connect runtimes by default" --body "..."
```

PR body: why (plain `connect` spawned unsandboxed runtimes), what (default-on + `--no-sandbox`, fail-closed startup probe, real server default spec), callouts (env scrubbing now applies by default; `--sandbox` flag removed; server placeholder default replaced with the cross-platform union spec; e2e opts out explicitly).

- [ ] **Step 3: Watch CI**

```bash
gh pr checks --watch
```

Expected: green before the work is considered done; fix any failures.
