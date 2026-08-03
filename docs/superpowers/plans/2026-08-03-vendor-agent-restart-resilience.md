# Vendor Agent Restart Resilience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restarting `horsie connect` stops destroying its sessions, and a long-lived vendor agent refreshes its own credential instead of 401-looping forever.

**Architecture:** The vendor's per-runtime directory becomes the source of truth — it gains `spec.json` (written by the vendor) and `agents.json` (written by the runtime process), so a `GetRuntime` for a runtime that is not live respawns it instead of reporting it gone. That behaviour is opt-in per vendor so velos, where a respawn would destroy work, is unaffected. Separately, `RuntimeVendor::run` takes a credential *provider* invoked per dial attempt rather than a string captured at startup.

**Tech Stack:** Rust (tokio, serde, futures), fluorite-generated models, tokio-tungstenite.

Spec: `docs/superpowers/specs/2026-08-03-vendor-agent-restart-resilience-design.md`

## Global Constraints

- Workspace lints deny `unwrap`, `expect`, and `panic` in production code; test modules opt out with the existing `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` block.
- Any change to `models/fluorite/*.fl` requires `make ts-types` before pushing — the CI drift job only diffs *tracked* files, so a new generated file passes silently and breaks the next PR.
- `cargo fmt` before `cargo clippy`; clippy runs with `-D warnings`.
- Part A (Tasks 1–4) must merge before Part B (Tasks 5–6): Part B adds a process-exit path that would destroy sessions without Part A.

## File Structure

**Part A — durable local runtimes**
- `runtime/src/state.rs` — gains optional file-backed persistence for the per-agent cwd/env map. Owns its own serde model; nothing else knows the file format.
- `runtime/src/main.rs:376` — constructs `RuntimeState` from the new `--state-file` argument.
- `models/fluorite/executor.fl:33` — `RuntimeConfig` gains `state_file: Option<String>`.
- `runtime-vendor/src/process_provider.rs:143` — passes `--state-file` to the child.
- `runtime-vendor/src/vendor.rs` — writes `spec.json`, respawns on a get-miss behind a flag, changes hibernate/delete semantics, grants the state dir in the caps file.
- `cli/src/connect.rs:276` — turns the flag on.

**Part B — credential provider**
- `runtime-vendor/src/vendor.rs` — `run` takes a `CredentialProvider`.
- `runtime-vendor/src/error.rs` — `CredentialError`.
- `cli/src/auth.rs:396` — `resolve_token` classifies transient vs dead.
- `cli/src/connect.rs:207`, `velos-runtime/src/main.rs:194` — supply providers.

---

### Task 1: Persist the runtime's per-agent cwd/env

**Files:**
- Modify: `runtime/src/state.rs`
- Test: `runtime/src/state.rs` (existing `mod tests`)

**Interfaces:**
- Produces: `RuntimeState::with_file(path: PathBuf) -> Self`, which loads an existing file if present and rewrites it after every mutation. `RuntimeState::new()` keeps today's in-memory-only behaviour.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn state_round_trips_through_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agents.json");

    let state = RuntimeState::with_file(path.clone());
    state.set_cwd("a", Some(PathBuf::from("/sub")));
    state.apply_env("a", "SET_VAR".into(), Some("1".into()));
    state.apply_env("a", "GONE_VAR".into(), None);

    // A fresh instance over the same file is the respawn case.
    let revived = RuntimeState::with_file(path);
    assert_eq!(
        revived.effective_dir("a", Path::new("/root")),
        PathBuf::from("/sub")
    );
    let overlay = revived.env_overlay("a");
    assert_eq!(overlay.sets, vec![("SET_VAR".to_string(), "1".to_string())]);
    assert_eq!(overlay.unsets, vec!["GONE_VAR".to_string()]);
}

#[test]
fn forget_is_persisted_too() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agents.json");
    let state = RuntimeState::with_file(path.clone());
    state.set_cwd("a", Some(PathBuf::from("/a")));
    state.set_cwd("b", Some(PathBuf::from("/b")));
    state.forget("a");
    assert_eq!(RuntimeState::with_file(path).tracked_agents(), 1);
}

/// A truncated or hand-edited file must not stop the runtime from starting:
/// losing a cwd override is an inconvenience, failing to boot is an outage.
#[test]
fn a_corrupt_file_starts_empty_rather_than_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agents.json");
    std::fs::write(&path, b"{not json").unwrap();
    let state = RuntimeState::with_file(path);
    assert_eq!(state.tracked_agents(), 0);
}

#[test]
fn an_in_memory_state_writes_nothing() {
    let state = RuntimeState::new();
    state.set_cwd("a", Some(PathBuf::from("/a")));
    assert_eq!(
        state.effective_dir("a", Path::new("/root")),
        PathBuf::from("/a")
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-runtime state::tests`
Expected: FAIL — no function `with_file`.

- [ ] **Step 3: Implement**

Add `serde::{Serialize, Deserialize}` derives to a persisted mirror of the map (`AgentEnv` itself gets the derives — its fields are already serde-friendly), a `file: Option<PathBuf>` on `RuntimeState`, and a `persist` helper called at the end of `set_cwd`, `apply_env`, and `forget` while the lock is still held:

```rust
#[derive(Default, Serialize, Deserialize)]
struct AgentEnv {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct RuntimeState {
    agents: Mutex<HashMap<String, AgentEnv>>,
    /// Where this map is mirrored, so a respawned runtime resumes with the
    /// agent's cwd and env intact. `None` keeps it purely in memory.
    file: Option<PathBuf>,
}

impl RuntimeState {
    /// A state map mirrored to `path`, loaded from it if it already exists.
    ///
    /// A file that cannot be read or parsed is treated as absent: a stale cwd
    /// override is worth losing, a runtime that refuses to start is not.
    #[must_use]
    pub fn with_file(path: PathBuf) -> Self {
        let agents = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            agents: Mutex::new(agents),
            file: Some(path),
        }
    }

    /// Mirror the map to disk. Best effort: the caller already holds the lock,
    /// and a write failure must not fail the tool call that triggered it.
    fn persist(&self, agents: &HashMap<String, AgentEnv>) {
        let Some(path) = &self.file else { return };
        if let Ok(bytes) = serde_json::to_vec(agents) {
            let _ = std::fs::write(path, bytes);
        }
    }
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-runtime state::tests`
Expected: PASS, including the four pre-existing tests.

- [ ] **Step 5: Commit**

```bash
git add runtime/src/state.rs
git commit -m "runtime: optionally persist per-agent cwd and env"
```

---

### Task 2: Pass the state-file path to the runtime child

**Files:**
- Modify: `models/fluorite/executor.fl:33`, `runtime/src/main.rs:38` (Cli) and `:376`, `runtime-vendor/src/process_provider.rs:143`
- Test: `runtime-vendor/src/process_provider.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `RuntimeState::with_file` from Task 1.
- Produces: `RuntimeConfig.state_file: Option<String>`; the runtime's `--state-file` argument.

- [ ] **Step 1: Add the model field**

In `models/fluorite/executor.fl`, inside `struct RuntimeConfig`:

```
    /// Where the runtime mirrors its per-agent cwd/env map, so a respawn
    /// resumes with it intact. Absent keeps that state in memory only.
    state_file: Option<String>,
```

- [ ] **Step 2: Regenerate and confirm the workspace still builds**

Run: `make ts-types && cargo build -p horsie-models`
Expected: regenerated TS under `clients/ts/src/generated`, clean build.

- [ ] **Step 3: Write the failing provider test**

```rust
#[test]
fn state_file_reaches_the_child_as_an_argument() {
    let config = RuntimeConfig {
        workspaces: vec![],
        plugins_dir: None,
        hook_path: vec![],
        env: vec![],
        provision: vec![],
        state_file: Some("/state/r1/agents.json".to_string()),
    };
    let args = child_args("horsie-runtime", "r1", "unix:/tmp/s.sock", &config, None);
    let i = args.iter().position(|a| a == "--state-file").expect("--state-file");
    assert_eq!(args[i + 1], "/state/r1/agents.json");
}
```

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test -p horsie-runtime-vendor process_provider`
Expected: FAIL — no `child_args`, no `state_file` field.

- [ ] **Step 5: Extract argv construction and add the argument**

Lift the argv building in `ProcessRuntimeProvider::create` (`process_provider.rs:138-156`) into a free function so it is testable without spawning:

```rust
/// The child's argv, minus the binary. Free-standing so the argument mapping
/// is testable without spawning a process.
fn child_args(
    _binary: &str,
    id: &str,
    endpoint: &str,
    config: &RuntimeConfig,
    caps: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--runtime-id".to_string(),
        id.to_string(),
    ];
    for ws in &config.workspaces {
        args.push("--workspace".to_string());
        args.push(format!("{}={}", ws.name, ws.path));
    }
    if let Some(dir) = &config.plugins_dir {
        args.push("--plugins-dir".to_string());
        args.push(dir.clone());
    }
    for hp in &config.hook_path {
        args.push("--hook-path".to_string());
        args.push(hp.clone());
    }
    if let Some(file) = &config.state_file {
        args.push("--state-file".to_string());
        args.push(file.clone());
    }
    if let Some(caps) = caps {
        args.push("--sandbox-caps".to_string());
        args.push(caps.display().to_string());
    }
    args
}
```

and call it from `create`: `cmd.args(child_args(...))`.

- [ ] **Step 6: Accept the argument in the runtime**

In `runtime/src/main.rs`, add to `struct Cli`:

```rust
    /// Where to mirror the per-agent cwd/env map, so a respawned runtime
    /// resumes with it intact. Absent keeps that state in memory only.
    #[arg(long = "state-file")]
    state_file: Option<PathBuf>,
```

and at `main.rs:376`:

```rust
    let state = Arc::new(match cli.state_file.clone() {
        Some(path) => horsie_runtime::state::RuntimeState::with_file(path),
        None => horsie_runtime::state::RuntimeState::new(),
    });
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p horsie-runtime-vendor process_provider && cargo build --workspace`
Expected: PASS, clean build.

- [ ] **Step 8: Commit**

```bash
git add models/fluorite/executor.fl clients/ts/src/generated runtime/src/main.rs runtime-vendor/src/process_provider.rs
git commit -m "runtime: accept --state-file and pass it from the process provider"
```

---

### Task 3: Persist the spec and respawn on a get-miss

**Files:**
- Modify: `runtime-vendor/src/vendor.rs` (`provision`, `dispatch`, `write_caps_file`, builder)
- Test: `runtime-vendor/tests/vendor_conformance.rs`

**Interfaces:**
- Consumes: `RuntimeConfig.state_file` from Task 2.
- Produces: `RuntimeVendor::with_respawnable_runtimes(bool) -> Self` (default false); `<state_dir>/<runtime_id>/spec.json`.

- [ ] **Step 1: Write the failing tests**

```rust
/// The bug this whole change exists for: a vendor agent restart must not
/// destroy the runtimes it was serving.
#[tokio::test]
async fn a_respawnable_runtime_survives_the_agent_process() {
    let h = harness().await;
    let first = h.vendor().with_respawnable_runtimes(true);
    first.create_runtime("r1", &spec(&["main"])).await.unwrap();
    drop(first);

    let second = h.vendor().with_respawnable_runtimes(true);
    second
        .get_runtime("r1")
        .await
        .expect("a get after an agent restart must respawn, not report it gone");
}

/// velos must keep today's semantics exactly: re-creating there means a fresh
/// container and a fresh clone, which silently destroys work.
#[tokio::test]
async fn a_non_respawnable_vendor_still_reports_it_gone() {
    let h = harness().await;
    let first = h.vendor();
    first.create_runtime("r1", &spec(&["main"])).await.unwrap();
    drop(first);

    let second = h.vendor();
    assert!(second.get_runtime("r1").await.is_err());
}

#[tokio::test]
async fn hibernate_stops_the_process_and_get_brings_it_back() {
    let h = harness().await;
    let v = h.vendor().with_respawnable_runtimes(true);
    v.create_runtime("r1", &spec(&["main"])).await.unwrap();
    v.hibernate_runtime("r1").await.unwrap();
    assert!(!v.is_live("r1").await, "hibernate must stop the process");
    v.get_runtime("r1").await.expect("a get resumes it");
    assert!(v.is_live("r1").await);
}

#[tokio::test]
async fn delete_removes_the_state_directory() {
    let h = harness().await;
    let v = h.vendor().with_respawnable_runtimes(true);
    v.create_runtime("r1", &spec(&["main"])).await.unwrap();
    assert!(h.state_dir().join("r1").join("spec.json").exists());
    v.delete_runtime("r1").await.unwrap();
    assert!(!h.state_dir().join("r1").exists());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-runtime-vendor --test vendor_conformance`
Expected: FAIL — no `with_respawnable_runtimes`.

- [ ] **Step 3: Implement the builder flag and spec persistence**

```rust
    /// Whether a get for a runtime that is not live may respawn it from its
    /// persisted spec.
    ///
    /// Off by default, and that default is load-bearing. A vendor that
    /// provisions its own workspace (velos) would re-clone on respawn,
    /// silently destroying work — for it a missing runtime is genuinely
    /// terminal. A vendor fixed to user-owned directories builds nothing, so
    /// respawning is just starting a process again.
    ///
    /// Deliberately not derived from `supports_provisioning`: "can you build a
    /// workspace?" and "is your runtime disposable?" are different questions.
    #[must_use]
    pub fn with_respawnable_runtimes(mut self, enabled: bool) -> Self {
        self.respawnable = enabled;
        self
    }

    fn spec_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("spec.json")
    }

    /// Where this runtime's process mirrors its per-agent cwd/env map. Written
    /// by the runtime, not by this agent — it only supplies the path.
    fn agents_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("agents.json")
    }

    /// Remember what this runtime was made of, so a later get can rebuild it
    /// without the server having to re-send anything.
    fn write_spec_file(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), String> {
        let path = self.spec_path(runtime_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create runtime state dir: {e}"))?;
        }
        let bytes = serde_json::to_vec(spec).map_err(|e| format!("encode runtime spec: {e}"))?;
        std::fs::write(&path, bytes).map_err(|e| format!("write runtime spec: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn read_spec_file(&self, runtime_id: &str) -> Option<RuntimeSpec> {
        let bytes = std::fs::read(self.spec_path(runtime_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
```

In `provision` (`vendor.rs:524`), write the spec when respawnable, and set the state file on the config:

```rust
        if self.respawnable {
            self.write_spec_file(runtime_id, request)?;
        }
        let config = RuntimeConfig {
            // ...existing fields...
            state_file: self
                .respawnable
                .then(|| self.agents_path(runtime_id).to_string_lossy().into_owned()),
        };
```

- [ ] **Step 4: Implement the three-step get**

Replace the `GetRuntime` arm (`vendor.rs:424-437`):

```rust
            RuntimeVendorCommand::GetRuntime(cmd) => {
                let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                let _guard = lock.lock().await;
                let live = self.transport_for(&cmd.runtime_id).await.is_some();
                // Live → hand it back. Not live but we know how to rebuild it →
                // rebuild. Neither → genuinely gone, which is what the server
                // turns into an unrecoverable session.
                let resolved = if live {
                    Ok(())
                } else {
                    match self.respawnable.then(|| self.read_spec_file(&cmd.runtime_id)).flatten() {
                        Some(spec) => self.provision(&cmd.runtime_id, &spec).await,
                        None => Err(format!(
                            "no runtime '{}' on this vendor; it cannot be resumed",
                            cmd.runtime_id
                        )),
                    }
                };
                resolved.map(|()| {
                    RuntimeVendorEvent::GetRuntime(GetRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    })
                })
            }
```

- [ ] **Step 5: Implement hibernate and delete**

`HibernateRuntime` stops the process but keeps the directory, and only for a respawnable vendor — one that cannot rebuild must keep declining, or hibernating would destroy the session:

```rust
            RuntimeVendorCommand::HibernateRuntime(cmd) => {
                if self.respawnable {
                    let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                    let _guard = lock.lock().await;
                    self.halt(&cmd.runtime_id).await;
                }
                Ok(RuntimeVendorEvent::HibernateRuntime(HibernateRuntimeResponse {
                    runtime_id: cmd.runtime_id,
                }))
            }
```

`DeleteRuntime` additionally removes the directory, after the existing `halt`:

```rust
                let _ = std::fs::remove_dir_all(self.state_dir.join(&cmd.runtime_id));
```

- [ ] **Step 6: Grant the state dir in the caps file**

In `write_caps_file` (`vendor.rs:630`), after the plugin-library grants:

```rust
        // The runtime writes its own per-agent cwd/env map here. The baseline
        // grants the working dir and system reads and nothing else, so without
        // this a sandboxed runtime's first set_env dies on a sandbox denial.
        spec.grants.push(Grant::Dir(DirGrant {
            path: self.state_dir.join(runtime_id).to_string_lossy().into_owned(),
            access: Access::ReadWrite,
        }));
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p horsie-runtime-vendor`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add runtime-vendor/src/vendor.rs runtime-vendor/tests/vendor_conformance.rs
git commit -m "runtime-vendor: respawn a runtime from its persisted spec"
```

---

### Task 4: Turn it on for `horsie connect`

**Files:**
- Modify: `cli/src/connect.rs:276`
- Test: `cli/tests/connect_e2e.rs`

**Interfaces:**
- Consumes: `with_respawnable_runtimes` from Task 3.

- [ ] **Step 1: Set the flag**

In `cli/src/connect.rs`, on the `RuntimeVendor::new(...)` builder chain:

```rust
    .with_sandbox(sandbox)
    // This agent's workspaces are the user's own directories, so a runtime is
    // just a process: killing it destroys nothing, and starting another is not
    // provisioning. Without this, Ctrl-C here would permanently kill every
    // session running on this machine.
    .with_respawnable_runtimes(true)
```

- [ ] **Step 2: Extend the e2e test**

In `cli/tests/connect_e2e.rs`, after the existing create-and-use flow, drop the agent, start a second one over the same state dir, and assert a get succeeds and a tool call still runs.

- [ ] **Step 3: Run tests**

Run: `cargo test -p horsie-cli --test connect_e2e`
Expected: PASS.

- [ ] **Step 4: Full check and commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add cli/src/connect.rs cli/tests/connect_e2e.rs
git commit -m "cli: local runtimes survive a horsie connect restart"
```

---

### Task 5: Resolve the credential per dial attempt

**Files:**
- Modify: `runtime-vendor/src/vendor.rs:268` (`run`), `runtime-vendor/src/error.rs`
- Test: `runtime-vendor/tests/vendor_conformance.rs`

**Interfaces:**
- Produces: `CredentialProvider`, `CredentialError::{Transient, Dead}`; `RuntimeVendor::run(&self, server_url: &str, credential: CredentialProvider, cancel: CancellationToken)`.

- [ ] **Step 1: Write the failing tests**

```rust
/// The 401 loop: a provider whose token goes stale must be asked again, not
/// have its first answer reused forever.
#[tokio::test]
async fn the_credential_is_resolved_on_every_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let provider: CredentialProvider = Arc::new(move || {
        let n = seen.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(Some(format!("token-{n}"))) })
    });
    // Server refuses the first two dials and accepts the third.
    let server = refusing_server(2).await;
    run_until_connected(&server, provider, &calls).await;
    assert!(calls.load(Ordering::SeqCst) >= 3, "each dial resolves afresh");
}

#[tokio::test]
async fn a_dead_credential_ends_the_run() {
    let provider: CredentialProvider =
        Arc::new(|| Box::pin(async { Err(CredentialError::Dead("logged out".into())) }));
    let err = vendor()
        .run("ws://127.0.0.1:1/api/vendor/connect", provider, CancellationToken::new())
        .await
        .expect_err("a dead credential must end the run, not loop");
    assert!(err.contains("logged out"), "{err}");
}

#[tokio::test]
async fn a_transient_credential_failure_keeps_retrying() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let provider: CredentialProvider = Arc::new(move || {
        let n = seen.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n < 2 {
                Err(CredentialError::Transient("issuer unreachable".into()))
            } else {
                Ok(Some("token".to_string()))
            }
        })
    });
    let server = refusing_server(0).await;
    run_until_connected(&server, provider, &calls).await;
    assert!(calls.load(Ordering::SeqCst) >= 3);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-runtime-vendor --test vendor_conformance credential`
Expected: FAIL — no `CredentialProvider`.

- [ ] **Step 3: Implement**

In `runtime-vendor/src/error.rs`:

```rust
/// Why a credential could not be produced, split by what the reconnect loop
/// should do about it.
#[derive(Debug)]
pub enum CredentialError {
    /// The issuer could not be reached. Indistinguishable from a failed dial,
    /// and retried the same way.
    Transient(String),
    /// The credential is definitively dead — revoked, or refused by the
    /// issuer. No amount of retrying will fix it, so the operator has to.
    Dead(String),
}

/// Resolves the bearer for one dial attempt. Called before *every* attempt
/// rather than once at startup: an access token outlives neither a long link
/// nor a long outage, and a link that is up is never re-authenticated, so a
/// captured token can be years stale by the time it is next presented.
pub type CredentialProvider = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Option<String>, CredentialError>> + Send>>
        + Send
        + Sync,
>;
```

In `run`, replace the `token: Option<&str>` parameter and the fail-fast check, then resolve inside the loop:

```rust
        // Reject an undialable URL before the first attempt. The token is no
        // longer checked here — it is resolved per attempt below.
        client_request(server_url, None)?;

        loop {
            let token = match credential().await {
                Ok(t) => t,
                Err(CredentialError::Dead(why)) => {
                    return Err(format!("credential rejected: {why}"));
                }
                Err(CredentialError::Transient(why)) => {
                    failures = failures.saturating_add(1);
                    let delay = backoff.next_delay();
                    note(&format!(
                        "vendor agent: attempt {failures} failed: {why}; reconnecting in {:.1}s",
                        delay.as_secs_f64()
                    ));
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            };
            let ended = match agent.connect(server_url, token.as_deref()).await {
                // ...unchanged...
            };
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-runtime-vendor`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime-vendor/src/vendor.rs runtime-vendor/src/error.rs runtime-vendor/tests/vendor_conformance.rs
git commit -m "runtime-vendor: resolve the credential on every dial attempt"
```

---

### Task 6: Supply the providers

**Files:**
- Modify: `cli/src/auth.rs:396`, `cli/src/connect.rs:207`, `velos-runtime/src/main.rs:194`
- Test: `cli/src/auth.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `CredentialProvider`, `CredentialError` from Task 5.
- Produces: `pub enum TokenOutcome { Token(Option<String>), Transient(String), Dead(String) }` and `resolve_token_outcome(server: &str) -> TokenOutcome`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_unreachable_issuer_is_transient_not_dead() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("credentials.json");
    let mut creds = Credentials::default();
    creds.set(
        "http://127.0.0.1:1",
        ServerCredentials {
            access_token: "hsk_usr_stale".into(),
            refresh_token: "hsk_ref_r".into(),
            expires_at: now_secs() - 1,
        },
    );
    creds.save(&path).unwrap();

    // Port 1 refuses the connection: the issuer is unreachable, which says
    // nothing about whether the credential is still good.
    match resolve_token_outcome_with("http://127.0.0.1:1", &path, None).await {
        TokenOutcome::Transient(_) => {}
        other => panic!("expected Transient, got {other:?}"),
    }
    assert!(
        Credentials::load(&path).unwrap().get("http://127.0.0.1:1").is_some(),
        "an unreachable issuer must not discard a credential that may still be valid"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-cli auth::tests`
Expected: FAIL — no `resolve_token_outcome_with`.

- [ ] **Step 3: Implement the classification**

Split `resolve_token_with`'s two failure paths (`cli/src/auth.rs:417-446`): the `?` on `post_json` becomes `TokenOutcome::Transient`, and the existing `Err(_)` arm — which already wipes the stored credential — becomes `TokenOutcome::Dead`. Keep `resolve_token` as a thin wrapper over the new function so `horsie auth status` and the other callers are untouched.

- [ ] **Step 4: Wire up both binaries**

`cli/src/connect.rs`, replacing the one-shot resolve at line 207:

```rust
    let server_for_token = server.to_string();
    let credential: CredentialProvider = Arc::new(move || {
        let server = server_for_token.clone();
        Box::pin(async move {
            match crate::auth::resolve_token_outcome(&server).await {
                TokenOutcome::Token(t) => Ok(t),
                TokenOutcome::Transient(m) => Err(CredentialError::Transient(m)),
                TokenOutcome::Dead(m) => Err(CredentialError::Dead(format!(
                    "{m} — run `horsie auth login --server {server}`"
                ))),
            }
        })
    });
```

The pre-flight `server_requires_auth` check stays: "you are not logged in" is still worth saying once, up front.

`velos-runtime/src/main.rs`, whose machine token never expires:

```rust
    let token = cli.token.clone();
    let credential: CredentialProvider = Arc::new(move || {
        let token = token.clone();
        Box::pin(async move { Ok(token) })
    });
```

- [ ] **Step 5: Run the full suite**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add cli/src/auth.rs cli/src/connect.rs velos-runtime/src/main.rs
git commit -m "cli: refresh the vendor agent's credential instead of 401-looping"
```
