# Runtime Env Tools Implementation Plan

> **For agentic workers:** Execute task-by-task inline in this session (user override: no subagents, no checkpoints). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `set_working_dir` and `set_env` runtime tools whose effects (cwd for all tools, env for `bash`) persist per session identity across future tool calls.

**Architecture:** A caller identity (`session_id`) is plumbed through `ToolCallRequest`; the runtime holds a `RuntimeState` keyed by it (cwd override + env overlay). Dispatch computes each call's effective base dir from the state; `bash` additionally applies the env overlay. Two new client-side tools expose this to agents; the server stamps its session id when building the agent toolbox.

**Tech Stack:** Rust, tokio, fluorite codegen (`models/fluorite/*.fl`), serde_json tool schemas.

Spec: `docs/superpowers/specs/2026-08-01-runtime-env-tools-design.md`

## Global Constraints

- Protocol types are ONLY defined in `models/fluorite/*.fl` (codegen). Never hand-write protocol structs.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. Mutex poisoning recovery uses `lock().unwrap_or_else(PoisonError::into_inner)` (allowed — not `unwrap`).
- Test modules open with the standard opt-out:
  ```rust
  #[cfg(test)]
  #[allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::wildcard_enum_match_arm
  )]
  mod tests {
  ```
- Unit tests live in-file under `#[cfg(test)] mod tests`, using `tempfile::TempDir`.
- Tool names are snake_case; the `ToolCall` variant tag doubles as the tool name.
- Avoid mutating process env (`std::env::set_var`) in tests — it is process-global and races with parallel tests.
- Pre-PR gates: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`.

---

### Task 1: Plumb session identity through the tool-call protocol

**Files:**
- Modify: `models/fluorite/runtime.fl` (ToolCallRequest gains `session_id`)
- Modify: `runtime-client/src/transport.rs` (trait signature)
- Modify: `runtime-client/src/client.rs` (stamp invokes)
- Modify: `runtime-client/src/testkit.rs` (MockTransport signature + session recording)
- Modify: `executor/src/socket_transport.rs` (thread into ToolCallRequest)
- Modify: `executor-client/src/ws_transport.rs` (thread into ToolCallRequest)

**Interfaces:**
- Produces: `RuntimeTransport::invoke(&self, call_id: &str, session_id: Option<&str>, call: ToolCall)`; `RuntimeClient::with_session_id(self, session_id: String) -> Self`; `MockTransport::session_ids()` / `TransportProbe::session_ids()` returning `Vec<Option<String>>`; `ToolCallRequest { call_id, session_id: Option<String>, call }`.

- [ ] **Step 1: Add `session_id` to the protocol**

In `models/fluorite/runtime.fl`, change:

```fl
struct ToolCallRequest  { call_id: String, session_id: Option<String>, call: ToolCall }
```

(with a comment: `// session_id keys the runtime's per-caller cwd/env state; absent = default bucket.`)

- [ ] **Step 2: Thread it through the transport trait and impls**

`runtime-client/src/transport.rs`:

```rust
async fn invoke(
    &self,
    call_id: &str,
    session_id: Option<&str>,
    call: ToolCall,
) -> Result<ToolResult, TransportError>;
```

`executor/src/socket_transport.rs` `invoke`: add the `session_id: Option<&str>` parameter and build:

```rust
let msg = RuntimeInboundMessage::ToolCall(ToolCallRequest {
    call_id: call_id.to_string(),
    session_id: session_id.map(str::to_string),
    call,
});
```

`executor-client/src/ws_transport.rs` `RelayRuntimeTransport::invoke`: same parameter, and in the `ToolCallRequest` literal add `session_id: session_id.map(str::to_string),`.

`runtime-client/src/testkit.rs` `MockTransport`: same signature; add a `sessions: Arc<Mutex<Vec<Option<String>>>>` field (initialized empty in `base`), record `session_id.map(str::to_string)` in `invoke`, and add:

```rust
/// The session id each invoke carried, in order.
pub fn session_ids(&self) -> Vec<Option<String>> {
    self.sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}
```

Give `TransportProbe` the same field + accessor, and wire it in `observed_by` (`self.sessions = probe.sessions.clone();`).

- [ ] **Step 3: Stamp invokes in RuntimeClient**

`runtime-client/src/client.rs`: add a `session_id: Option<String>` field to `RuntimeClient` (`None` in `new`/`from_arc`); `#[derive(Clone)]` copies it. Add:

```rust
/// Stamp every invoke with this caller identity; the runtime keys its
/// per-caller cwd/env state by it. Cheap — shares the inner Arcs.
#[must_use]
pub fn with_session_id(self, session_id: String) -> Self {
    Self {
        session_id: Some(session_id),
        ..self
    }
}
```

In `invoke`, pass `self.session_id.as_deref()` as the new argument.

- [ ] **Step 4: Fix remaining call sites and write the stamping test**

Direct `transport.invoke(...)` callers gain the new arg: `runtime-client/src/testkit.rs` tests and `executor/src/socket_transport.rs` tests pass `None`.

New test in `runtime-client/src/client.rs` tests:

```rust
#[tokio::test]
async fn session_id_is_stamped_on_invokes() {
    let probe = crate::testkit::TransportProbe::new();
    let client = RuntimeClient::new(MockTransport::ok("").observed_by(&probe))
        .with_session_id("sess-1".into());
    client.invoke(probe_call()).await.unwrap();
    assert_eq!(probe.session_ids(), vec![Some("sess-1".to_string())]);
}

#[tokio::test]
async fn an_unstamped_client_sends_no_session_id() {
    let probe = crate::testkit::TransportProbe::new();
    let client = RuntimeClient::new(MockTransport::ok("").observed_by(&probe));
    client.invoke(probe_call()).await.unwrap();
    assert_eq!(probe.session_ids(), vec![None]);
}
```

- [ ] **Step 5: Build and test**

Run: `cargo test -p horsie-models -p horsie-runtime-client -p horsie-executor -p horsie-executor-client`
Expected: PASS (crate names: verify via `grep '^name' runtime-client/Cargo.toml executor-client/Cargo.toml executor/Cargo.toml` first).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "Plumb a session identity through tool-call requests"
```

---

### Task 2: `RuntimeState` — per-caller cwd/env state

**Files:**
- Create: `runtime/src/state.rs`
- Modify: `runtime/src/lib.rs` (add `pub mod state;` after `pub mod scan;` — keep alphabetical: plugins, plugins_fetch, provision, scan, state, steps, tools, workspace)

**Interfaces:**
- Produces: `RuntimeState::{new, effective_dir, set_cwd, apply_env, env_overlay}`, `EnvOverlay { sets, unsets }`, `EnvOverlay::apply_to(&self, &mut tokio::process::Command)`. Consumed by Task 3.

- [ ] **Step 1: Write `runtime/src/state.rs` with tests**

```rust
//! Per-caller shell-like state for tool execution: a working-directory
//! override and an env overlay, keyed by the session id stamped on each tool
//! call. Callers sharing one runtime (an agent and its subagents, sessions on
//! a shared local daemon) are isolated by identity; unidentified callers
//! (`None`) share a default bucket. Entries live for the runtime process's
//! lifetime — bounded by the number of distinct callers attaching to it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Env changes to apply to a spawned command. Named fields so call sites
/// can't swap sets and unsets.
#[derive(Default)]
pub struct EnvOverlay {
    pub sets: Vec<(String, String)>,
    pub unsets: Vec<String>,
}

impl EnvOverlay {
    /// Apply the overlay to a child process command: sets win over the
    /// inherited environment, unsets remove even inherited variables.
    pub fn apply_to(&self, command: &mut tokio::process::Command) {
        for (name, value) in &self.sets {
            command.env(name, value);
        }
        for name in &self.unsets {
            command.env_remove(name);
        }
    }
}

#[derive(Default)]
struct SessionEnv {
    /// Working-directory override; `None` = resolve per call from `workspace`.
    cwd: Option<PathBuf>,
    /// Env overlay: `Some(v)` = set, `None` = unset.
    env: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct RuntimeState {
    sessions: Mutex<HashMap<Option<String>, SessionEnv>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The caller's cwd override if set, else `fallback`.
    pub fn effective_dir(&self, session: &Option<String>, fallback: &Path) -> PathBuf {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        match sessions.get(session).and_then(|s| s.cwd.clone()) {
            Some(dir) => dir,
            None => fallback.to_path_buf(),
        }
    }

    /// Store (`Some`) or clear (`None`) the caller's cwd override.
    pub fn set_cwd(&self, session: &Option<String>, dir: Option<PathBuf>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.entry(session.clone()).or_default().cwd = dir;
    }

    /// Record an env set (`Some`) or unset (`None`) for the caller.
    pub fn apply_env(&self, session: &Option<String>, name: String, value: Option<String>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions
            .entry(session.clone())
            .or_default()
            .env
            .insert(name, value);
    }

    /// The caller's accumulated env overlay.
    pub fn env_overlay(&self, session: &Option<String>) -> EnvOverlay {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let mut overlay = EnvOverlay::default();
        if let Some(env) = sessions.get(session) {
            for (name, value) in &env.env {
                match value {
                    Some(v) => overlay.sets.push((name.clone(), v.clone())),
                    None => overlay.unsets.push(name.clone()),
                }
            }
        }
        overlay
    }
}
```

Tests (same file, standard opt-out header):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn s(name: &str) -> Option<String> {
        Some(name.to_string())
    }

    #[test]
    fn no_override_returns_the_fallback() {
        let state = RuntimeState::new();
        assert_eq!(
            state.effective_dir(&None, Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    #[test]
    fn cwd_override_wins_and_reset_clears_it() {
        let state = RuntimeState::new();
        state.set_cwd(&s("a"), Some(PathBuf::from("/sub")));
        assert_eq!(
            state.effective_dir(&s("a"), Path::new("/root")),
            PathBuf::from("/sub")
        );
        state.set_cwd(&s("a"), None);
        assert_eq!(
            state.effective_dir(&s("a"), Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    #[test]
    fn sessions_are_isolated_and_none_is_the_default_bucket() {
        let state = RuntimeState::new();
        state.set_cwd(&s("a"), Some(PathBuf::from("/a")));
        state.set_cwd(&None, Some(PathBuf::from("/default")));
        assert_eq!(
            state.effective_dir(&s("b"), Path::new("/root")),
            PathBuf::from("/root")
        );
        assert_eq!(
            state.effective_dir(&None, Path::new("/root")),
            PathBuf::from("/default")
        );
    }

    #[test]
    fn env_overlay_accumulates_sets_and_unsets_per_session() {
        let state = RuntimeState::new();
        state.apply_env(&s("a"), "SET_VAR".into(), Some("1".into()));
        state.apply_env(&s("a"), "GONE_VAR".into(), None);
        let overlay = state.env_overlay(&s("a"));
        assert_eq!(overlay.sets, vec![("SET_VAR".to_string(), "1".to_string())]);
        assert_eq!(overlay.unsets, vec!["GONE_VAR".to_string()]);
        let other = state.env_overlay(&s("b"));
        assert!(other.sets.is_empty() && other.unsets.is_empty());
    }
}
```

(Note: `sets` order — one entry, so vec equality is deterministic. With two sets, sort before asserting.)

- [ ] **Step 2: Register the module**

Add `pub mod state;` to `runtime/src/lib.rs` (alphabetical, after `pub mod scan;`).

- [ ] **Step 3: Test**

Run: `cargo test -p horsie-runtime state` (verify crate name via `grep '^name' runtime/Cargo.toml`)
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add runtime/src/state.rs runtime/src/lib.rs
git commit -m "Add per-caller runtime state for cwd and env"
```

---

### Task 3: Protocol variants + `set_working_dir` / `set_env` runtime tools + dispatch rewiring

**Files:**
- Modify: `models/fluorite/runtime.fl` (inputs + ToolCall variants)
- Create: `runtime/src/tools/set_working_dir.rs`
- Create: `runtime/src/tools/set_env.rs`
- Modify: `runtime/src/tools/mod.rs` (dispatch signature, new arms, effective dir)
- Modify: `runtime/src/tools/bash.rs` (`env: &EnvOverlay` param)
- Modify: `runtime/src/main.rs` (create state, pass `req.session_id`)

**Interfaces:**
- Consumes: `RuntimeState`, `EnvOverlay` (Task 2); `ToolCallRequest.session_id` (Task 1).
- Produces: `dispatch(registry: &WorkspaceRegistry, state: &RuntimeState, session: &Option<String>, call: ToolCall) -> ToolResult`; `bash::exec(working_dir: &Path, env: &EnvOverlay, input: BashInput) -> ToolResult`; `set_working_dir::exec(registry, state, session, input) -> ToolResult`; `set_env::exec(state, session, input) -> ToolResult`. `ToolCall::SetWorkingDir(SetWorkingDirInput { path: Option<String>, workspace: Option<String> })`, `ToolCall::SetEnv(SetEnvInput { name: String, value: Option<String> })` — consumed by Task 5.

- [ ] **Step 1: Add the protocol types**

In `models/fluorite/runtime.fl`, after `GrepInput`:

```fl
// Set the caller's working directory for all future tool calls. `path` may be
// absolute or relative to the current effective cwd; omit it to reset to
// per-workspace resolution (`workspace` then names the reset target).
struct SetWorkingDirInput { path: Option<String>, workspace: Option<String> }

// Set (`value` present) or unset (`value` absent) an env var for the caller's
// future bash commands. Runtime-global state would leak across sessions, so
// this is keyed by ToolCallRequest.session_id like the cwd override.
struct SetEnvInput { name: String, value: Option<String> }
```

Add to the `ToolCall` union:

```fl
    SetWorkingDir(SetWorkingDirInput),
    SetEnv(SetEnvInput),
```

- [ ] **Step 2: `runtime/src/tools/set_working_dir.rs`**

```rust
use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;
use horsie_models::runtime::{SetWorkingDirInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub fn exec(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    session: &Option<String>,
    input: SetWorkingDirInput,
) -> ToolResult {
    match &input.path {
        Some(path) => set(registry, state, session, &input.workspace, path),
        None => reset(registry, state, session, &input.workspace),
    }
}

/// Point the caller's cwd at `path` — absolute, or relative to the caller's
/// current effective cwd. A bad target is an error and changes nothing.
fn set(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    session: &Option<String>,
    workspace: &Option<String>,
    path: &str,
) -> ToolResult {
    let base = match registry.resolve(workspace) {
        Ok(root) => state.effective_dir(session, &root),
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    // Path::join discards the base when `path` is absolute — exactly cd semantics.
    let candidate = base.join(Path::new(path));
    let dir = match candidate.canonicalize() {
        Ok(d) => d,
        Err(e) => {
            return ToolResult::Err(ToolError {
                reason: format!("cannot set working directory to '{path}': {e}"),
            });
        }
    };
    if !dir.is_dir() {
        return ToolResult::Err(ToolError {
            reason: format!("not a directory: {}", dir.display()),
        });
    }
    state.set_cwd(session, Some(dir.clone()));
    ok(dir.display().to_string())
}

/// Clear the caller's override, returning to per-call workspace resolution.
/// The target workspace is validated first so a typo doesn't silently drop
/// the override.
fn reset(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    session: &Option<String>,
    workspace: &Option<String>,
) -> ToolResult {
    let root = match registry.resolve(workspace) {
        Ok(r) => r,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    state.set_cwd(session, None);
    ok(root.display().to_string())
}

fn ok(stdout: String) -> ToolResult {
    ToolResult::Ok(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: 0,
    })
}
```

Tests (same file, standard header):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::Workspace;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, WorkspaceRegistry, RuntimeState) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        (dir, registry, RuntimeState::new())
    }

    fn input(path: Option<&str>, workspace: Option<&str>) -> SetWorkingDirInput {
        SetWorkingDirInput {
            path: path.map(str::to_string),
            workspace: workspace.map(str::to_string),
        }
    }

    #[test]
    fn relative_path_resolves_against_current_cwd_and_chains() {
        let (dir, registry, state) = fixture();
        std::fs::create_dir(dir.path().join("sub/deep")).unwrap();
        let session = None;
        let r = exec(&registry, &state, &session, input(Some("sub"), None));
        match r {
            ToolResult::Ok(o) => {
                assert_eq!(o.stdout, dir.path().join("sub").canonicalize().unwrap().display().to_string())
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        // A second relative set chains off the first.
        let r = exec(&registry, &state, &session, input(Some("deep"), None));
        assert!(matches!(r, ToolResult::Ok(_)));
        assert_eq!(
            state.effective_dir(&session, dir.path()),
            dir.path().join("sub/deep").canonicalize().unwrap()
        );
    }

    #[test]
    fn absolute_path_is_used_as_is() {
        let (dir, registry, state) = fixture();
        let abs = dir.path().join("sub").display().to_string();
        let r = exec(&registry, &state, &None, input(Some(&abs), None));
        assert!(matches!(r, ToolResult::Ok(_)));
    }

    #[test]
    fn nonexistent_target_errors_and_preserves_state() {
        let (dir, registry, state) = fixture();
        let session = None;
        let r = exec(&registry, &state, &session, input(Some("nope"), None));
        assert!(matches!(r, ToolResult::Err(_)));
        assert_eq!(state.effective_dir(&session, dir.path()), dir.path());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let (dir, registry, state) = fixture();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let r = exec(&registry, &state, &None, input(Some("f.txt"), None));
        match r {
            ToolResult::Err(e) => assert!(e.reason.contains("not a directory"), "{}", e.reason),
            ToolResult::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn reset_clears_the_override() {
        let (dir, registry, state) = fixture();
        let session = None;
        let _ = exec(&registry, &state, &session, input(Some("sub"), None));
        let r = exec(&registry, &state, &session, input(None, None));
        assert!(matches!(r, ToolResult::Ok(_)));
        assert_eq!(state.effective_dir(&session, dir.path()), dir.path());
    }

    #[test]
    fn reset_with_unknown_workspace_errors_and_keeps_the_override() {
        let (dir, registry, state) = fixture();
        let session = None;
        let _ = exec(&registry, &state, &session, input(Some("sub"), None));
        let r = exec(&registry, &state, &session, input(None, Some("zzz")));
        assert!(matches!(r, ToolResult::Err(_)));
        assert_eq!(
            state.effective_dir(&session, dir.path()),
            dir.path().join("sub").canonicalize().unwrap()
        );
    }
}
```

- [ ] **Step 3: `runtime/src/tools/set_env.rs`**

```rust
use crate::state::RuntimeState;
use horsie_models::runtime::{SetEnvInput, ToolError, ToolOutput, ToolResult};

/// Record an env set (`value` present) or unset (absent) for the caller's
/// future bash commands. The value is never echoed back — confirmations name
/// only the variable, so secrets don't land in the conversation history.
pub fn exec(state: &RuntimeState, session: &Option<String>, input: SetEnvInput) -> ToolResult {
    if input.name.is_empty() || input.name.contains(['=', '\0']) {
        return ToolResult::Err(ToolError {
            reason: format!("invalid environment variable name: '{}'", input.name),
        });
    }
    if let Some(value) = &input.value {
        if value.contains('\0') {
            return ToolResult::Err(ToolError {
                reason: "environment variable value contains NUL".to_string(),
            });
        }
    }
    let verb = if input.value.is_some() { "set" } else { "unset" };
    let name = input.name.clone();
    state.apply_env(session, input.name, input.value);
    ToolResult::Ok(ToolOutput {
        stdout: format!("{verb} {name}"),
        stderr: String::new(),
        exit_code: 0,
    })
}
```

Tests (same file, standard header):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn input(name: &str, value: Option<&str>) -> SetEnvInput {
        SetEnvInput {
            name: name.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn set_is_recorded_and_confirmed_without_the_value() {
        let state = RuntimeState::new();
        let r = exec(&state, &None, input("TOKEN", Some("s3cret")));
        match r {
            ToolResult::Ok(o) => {
                assert_eq!(o.stdout, "set TOKEN");
                assert!(!o.stdout.contains("s3cret"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        assert_eq!(
            state.env_overlay(&None).sets,
            vec![("TOKEN".to_string(), "s3cret".to_string())]
        );
    }

    #[test]
    fn unset_is_recorded() {
        let state = RuntimeState::new();
        let r = exec(&state, &None, input("TOKEN", None));
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "unset TOKEN"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        assert_eq!(state.env_overlay(&None).unsets, vec!["TOKEN".to_string()]);
    }

    #[test]
    fn invalid_names_are_rejected_and_change_nothing() {
        let state = RuntimeState::new();
        for bad in ["", "A=B", "A\0B"] {
            let r = exec(&state, &None, input(bad, Some("1")));
            assert!(matches!(r, ToolResult::Err(_)), "accepted '{bad:?}'");
        }
        assert!(state.env_overlay(&None).sets.is_empty());
    }

    #[test]
    fn nul_in_value_is_rejected() {
        let state = RuntimeState::new();
        let r = exec(&state, &None, input("A", Some("x\0y")));
        assert!(matches!(r, ToolResult::Err(_)));
    }
}
```

- [ ] **Step 4: Rewire `dispatch` in `runtime/src/tools/mod.rs`**

Add `pub mod set_env;` and `pub mod set_working_dir;` (alphabetical), plus `use crate::state::RuntimeState;`. Replace `workspace_of` and `dispatch`:

```rust
/// `SetEnv` carries no `workspace`; match exhaustiveness (wildcards are
/// lint-denied) still needs an arm, over this const.
const NONE: Option<String> = None;

fn workspace_of(call: &ToolCall) -> &Option<String> {
    match call {
        ToolCall::Bash(i) => &i.workspace,
        ToolCall::ReadFile(i) => &i.workspace,
        ToolCall::WriteFile(i) => &i.workspace,
        ToolCall::FindAndReplace(i) => &i.workspace,
        ToolCall::ReplaceLines(i) => &i.workspace,
        ToolCall::ListFiles(i) => &i.workspace,
        ToolCall::Glob(i) => &i.workspace,
        ToolCall::Grep(i) => &i.workspace,
        ToolCall::SetWorkingDir(i) => &i.workspace,
        ToolCall::SetEnv(_) => &NONE,
    }
}

/// Resolve the call's target workspace to a root directory (the single translation
/// site), apply the caller's cwd override if it has one, run the tool there, then
/// clamp its output. The state-mutating variants run against the registry + state
/// instead of a directory. An unresolvable `workspace` (missing with several
/// workspaces, or an unknown name) is returned to the model as a `ToolError`.
pub async fn dispatch(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    session: &Option<String>,
    call: ToolCall,
) -> ToolResult {
    let result = match call {
        ToolCall::SetWorkingDir(input) => set_working_dir::exec(registry, state, session, input),
        ToolCall::SetEnv(input) => set_env::exec(state, session, input),
        call => {
            let dir = match registry.resolve(workspace_of(&call)) {
                Ok(d) => state.effective_dir(session, &d),
                Err(reason) => return ToolResult::Err(ToolError { reason }),
            };
            match call {
                ToolCall::Bash(input) => bash::exec(&dir, &state.env_overlay(session), input).await,
                ToolCall::ReadFile(input) => read_file::exec(&dir, input).await,
                ToolCall::WriteFile(input) => write_file::exec(&dir, input).await,
                ToolCall::FindAndReplace(input) => find_and_replace::exec(&dir, input).await,
                ToolCall::ReplaceLines(input) => replace_lines::exec(&dir, input).await,
                ToolCall::ListFiles(input) => list_files::exec(&dir, input).await,
                ToolCall::Glob(input) => glob::exec(&dir, input).await,
                ToolCall::Grep(input) => grep::exec(&dir, input).await,
                ToolCall::SetWorkingDir(_) | ToolCall::SetEnv(_) => ToolResult::Err(ToolError {
                    // Dead: the outer match routed both variants before this arm.
                    reason: "internal dispatch error".to_string(),
                }),
            }
        }
    };
    // ... existing clamp unchanged ...
}
```

Update the two existing dispatch tests to the new signature (`&RuntimeState::new(), &None` after the registry) and add:

```rust
#[tokio::test]
async fn file_tools_follow_the_session_cwd() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/f.txt"), "nested").unwrap();
    let registry = WorkspaceRegistry::new(vec![Workspace {
        name: "ws".into(),
        path: dir.path().to_path_buf(),
    }]);
    let state = RuntimeState::new();
    let session = None;
    let r = dispatch(
        &registry,
        &state,
        &session,
        ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
            path: Some("sub".into()),
            workspace: None,
        }),
    )
    .await;
    assert!(matches!(r, ToolResult::Ok(_)));
    let r = dispatch(
        &registry,
        &state,
        &session,
        ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
            path: "f.txt".into(),
            start_line: None,
            end_line: None,
            workspace: None,
        }),
    )
    .await;
    match r {
        ToolResult::Ok(o) => assert_eq!(o.stdout, "nested"),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}

#[tokio::test]
async fn cwd_overrides_are_isolated_per_session() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let registry = WorkspaceRegistry::new(vec![Workspace {
        name: "ws".into(),
        path: dir.path().to_path_buf(),
    }]);
    let state = RuntimeState::new();
    let a = Some("a".to_string());
    let b = Some("b".to_string());
    let r = dispatch(
        &registry,
        &state,
        &a,
        ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
            path: Some("sub".into()),
            workspace: None,
        }),
    )
    .await;
    assert!(matches!(r, ToolResult::Ok(_)));
    // Session b still resolves relative paths against the workspace root:
    // it can read a file that session a's cwd (sub/) does not contain.
    std::fs::write(dir.path().join("root.txt"), "at root").unwrap();
    let r = dispatch(
        &registry,
        &state,
        &b,
        ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
            path: "root.txt".into(),
            start_line: None,
            end_line: None,
            workspace: None,
        }),
    )
    .await;
    match r {
        ToolResult::Ok(o) => assert_eq!(o.stdout, "at root"),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}
```

- [ ] **Step 5: `bash.rs` env overlay**

Change the signature to `pub async fn exec(working_dir: &Path, env: &crate::state::EnvOverlay, input: BashInput) -> ToolResult`, and after the `command` builder chain (before spawn) add `env.apply_to(&mut command);`. Update every test call site to insert `&crate::state::EnvOverlay::default(),` as the middle argument. Add one test:

```rust
#[tokio::test]
async fn env_overlay_reaches_the_child() {
    let dir = TempDir::new().unwrap();
    let overlay = crate::state::EnvOverlay {
        sets: vec![("HORSIE_TEST_VAR".to_string(), "hello".to_string())],
        unsets: vec![],
    };
    let result = exec(
        dir.path(),
        &overlay,
        BashInput {
            command: "echo $HORSIE_TEST_VAR".to_string(),
            timeout_secs: None,
            workspace: None,
        },
    )
    .await;
    match result {
        ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "hello"),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}
```

- [ ] **Step 6: Create and pass state in `runtime/src/main.rs`**

In `run_loop`, before the `while let` loop:

```rust
let state = Arc::new(horsie_runtime::state::RuntimeState::new());
```

In the `RuntimeInboundMessage::ToolCall(req)` arm, clone it like `registry` and change the dispatch call:

```rust
let state = state.clone();
...
let result =
    horsie_runtime::tools::dispatch(&registry, &state, &req.session_id, req.call).await;
```

(`req.session_id` is borrowed inside the spawned task — bind `let session_id = req.session_id.clone();` before the spawn and move it in, since `req.call` is moved separately.)

- [ ] **Step 7: Build and test the runtime**

Run: `cargo test -p horsie-runtime -p horsie-models`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "Add set_working_dir and set_env runtime tools keyed per session"
```

---

### Task 4: Server stamps the session id on its runtime client

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (~line 1000, `SessionContextProvider::provide`)

**Interfaces:**
- Consumes: `RuntimeClient::with_session_id` (Task 1).

- [ ] **Step 1: Stamp at the one production assembly point**

In `SessionContextProvider::provide`, change the `DefaultToolboxFactory.for_agent` call's client argument to:

```rust
self.runtime_client
    .clone()
    .with_session_id(self.session_id.to_string()),
```

- [ ] **Step 2: Build and test the server**

Run: `cargo test -p horsie-server` (verify crate name via `grep '^name' server/Cargo.toml`)
Expected: PASS (existing session tests exercise this path; the stamping itself is covered by Task 1's client tests and Task 3's runtime isolation tests)

- [ ] **Step 3: Commit**

```bash
git add server/src/sessions/session_actor.rs
git commit -m "Stamp the session id on runtime tool calls"
```

---

### Task 5: Agent-facing client tools

**Files:**
- Create: `runtime-client/src/tools/set_working_dir.rs`
- Create: `runtime-client/src/tools/set_env.rs`
- Modify: `runtime-client/src/tools/mod.rs` (modules, re-exports, registration)

**Interfaces:**
- Consumes: `ToolCall::SetWorkingDir` / `ToolCall::SetEnv` (Task 3), `MockTransport` (Task 1).
- Produces: `SetWorkingDirTool`, `SetEnvTool`, registered in `add_runtime_tools`.

- [ ] **Step 1: `runtime-client/src/tools/set_working_dir.rs`**

```rust
use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{SetWorkingDirInput, ToolCall};
use serde_json::{Value, json};

pub struct SetWorkingDirTool {
    client: RuntimeClient,
}
impl SetWorkingDirTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for SetWorkingDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_working_dir".to_string(),
            description: "Set the working directory for all future tool calls in this \
                session — bash commands and relative paths in the file tools alike. \
                'path' may be absolute or relative to the current working directory. \
                Omit 'path' to reset to per-workspace resolution (name a 'workspace' \
                to choose which when there are several). Persists until reset; other \
                sessions sharing this runtime are unaffected. Returns the new working \
                directory."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "workspace": {
                        "type": "string",
                        "description": "Reset resolution to this workspace's root (see '# Workspaces'). Only used when 'path' is omitted."
                    }
                }
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"].as_str().map(str::to_string);
        let workspace = crate::tools::workspace_arg(&input);
        self.client
            .invoke(ToolCall::SetWorkingDir(SetWorkingDirInput { path, workspace }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}
```

Tests (same file, standard header), following `bash.rs`'s pattern:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::testkit::MockTransport;

    #[tokio::test]
    async fn forwards_path_and_workspace() {
        let t = MockTransport::ok("/ws/sub");
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetWorkingDirTool::new(RuntimeClient::new(t.observed_by(&probe)));
        let v = tool
            .execute(json!({"path": "sub", "workspace": "ws"}))
            .await
            .unwrap();
        assert_eq!(v.as_str().unwrap(), "/ws/sub");
        match &probe.invocations()[0] {
            ToolCall::SetWorkingDir(i) => {
                assert_eq!(i.path.as_deref(), Some("sub"));
                assert_eq!(i.workspace.as_deref(), Some("ws"));
            }
            other => panic!("expected SetWorkingDir, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn omitted_fields_stay_none_for_reset() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetWorkingDirTool::new(RuntimeClient::new(
            MockTransport::ok("").observed_by(&probe),
        ));
        tool.execute(json!({})).await.unwrap();
        match &probe.invocations()[0] {
            ToolCall::SetWorkingDir(i) => {
                assert!(i.path.is_none() && i.workspace.is_none())
            }
            other => panic!("expected SetWorkingDir, got {other:?}"),
        }
    }

    #[test]
    fn spec_shape() {
        let tool = SetWorkingDirTool::new(RuntimeClient::new(MockTransport::ok("")));
        let spec = tool.spec();
        assert_eq!(spec.name, "set_working_dir");
        assert!(spec.input_schema["required"].is_null());
    }
}
```

- [ ] **Step 2: `runtime-client/src/tools/set_env.rs`**

```rust
use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{SetEnvInput, ToolCall};
use serde_json::{Value, json};

pub struct SetEnvTool {
    client: RuntimeClient,
}
impl SetEnvTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for SetEnvTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_env".to_string(),
            description: "Set or unset an environment variable for this session's future \
                bash commands. Omit 'value' to unset — the variable is removed even if \
                the runtime process defines it. Persists until changed again; file tools \
                are unaffected, and so are other sessions sharing this runtime."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "value": { "type": "string" }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'name'".into()))?
            .to_string();
        let value = input["value"].as_str().map(str::to_string);
        self.client
            .invoke(ToolCall::SetEnv(SetEnvInput { name, value }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}
```

Tests (same file, standard header):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::testkit::MockTransport;

    #[tokio::test]
    async fn forwards_name_and_value() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetEnvTool::new(RuntimeClient::new(
            MockTransport::ok("set FOO").observed_by(&probe),
        ));
        let v = tool.execute(json!({"name": "FOO", "value": "1"})).await.unwrap();
        assert_eq!(v.as_str().unwrap(), "set FOO");
        match &probe.invocations()[0] {
            ToolCall::SetEnv(i) => {
                assert_eq!(i.name, "FOO");
                assert_eq!(i.value.as_deref(), Some("1"));
            }
            other => panic!("expected SetEnv, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn omitted_value_means_unset() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetEnvTool::new(RuntimeClient::new(
            MockTransport::ok("").observed_by(&probe),
        ));
        tool.execute(json!({"name": "FOO"})).await.unwrap();
        match &probe.invocations()[0] {
            ToolCall::SetEnv(i) => assert!(i.value.is_none()),
            other => panic!("expected SetEnv, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_name_is_an_input_error() {
        let tool = SetEnvTool::new(RuntimeClient::new(MockTransport::ok("")));
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn spec_shape() {
        let tool = SetEnvTool::new(RuntimeClient::new(MockTransport::ok("")));
        let spec = tool.spec();
        assert_eq!(spec.name, "set_env");
        assert_eq!(spec.input_schema["required"], json!(["name"]));
        assert!(spec.input_schema["properties"].get("workspace").is_none());
    }
}
```

- [ ] **Step 3: Register in `runtime-client/src/tools/mod.rs`**

Add `mod set_env;` / `mod set_working_dir;` (alphabetical), `pub use set_env::SetEnvTool;` / `pub use set_working_dir::SetWorkingDirTool;`, and in `add_runtime_tools` change the tail to:

```rust
        .add(GlobTool::new(client.clone()))
        .add(GrepTool::new(client.clone()))
        .add(SetWorkingDirTool::new(client.clone()))
        .add(SetEnvTool::new(client))
```

- [ ] **Step 4: Test**

Run: `cargo test -p horsie-runtime-client`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add runtime-client/src/tools
git commit -m "Expose set_working_dir and set_env to agents"
```

---

### Task 6: Full verification and PR

- [ ] **Step 1: Run the pre-PR gates**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```

Fix anything they surface before proceeding.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin runtime-env-tools
gh pr create --title "Add set_working_dir and set_env runtime tools with per-session state" --body "..."
```

Body: why (agents repeat `cd`/`VAR=` prefixes; file tools can't follow), what (two tools; cwd applies to all tools, env to bash; state keyed by a session id newly plumbed through `ToolCallRequest`; server stamps its session id), key callouts (per-session isolation supports future agent+subagent runtimes; unidentified callers share a default bucket; `set_env` never echoes values; sandbox unchanged).

- [ ] **Step 3: Watch CI until green**

`gh pr checks --watch`; fix any failures and push again until the PR is green and mergeable.
