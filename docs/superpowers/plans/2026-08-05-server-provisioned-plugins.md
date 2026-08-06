# Server-provisioned plugins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove plugin and marketplace management from the CLI, and make a session's server-selected bundles the only skills a runtime sees — materialized once per runtime, cleaned up when the runtime is deleted.

**Architecture:** The runtime already fetches its session's bundles from the server at startup. This makes that directory per-runtime instead of shared, makes materialization happen once per runtime identity instead of once per process (via a manifest marker), deletes the CLI-side plugin library that competed with it, and grants the sandbox the directory it has always needed.

**Tech Stack:** Rust (workspace crates `horsie-models`, `horsie-runtime`, `horsie-runtime-vendor`, `horsie-support`, `horsie` CLI), fluorite schemas under `models/fluorite/`.

Spec: `docs/superpowers/specs/2026-08-05-server-provisioned-plugins-design.md`.

## Global Constraints

- Workspace lints deny `unwrap`, `expect` and `panic` in production code. Test modules opt out with the existing `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` attribute on `mod tests`.
- Run `cargo fmt` **before** `cargo clippy`; clippy reports formatting-sensitive lints that a later fmt would churn.
- Iterate with `cargo test -p <crate> --lib`. Run the full `make check` once, at the end, before pushing.
- After editing any file under `models/fluorite/`, run `touch models/build.rs` or the change is not regenerated.
- `models/fluorite/executor.fl` has no TypeScript consumer (there is no `executor` directory under `clients/ts/src/generated/`), so no type regeneration is needed for it.
- Commit messages: short subject, no body unless the diff hides context. Never list Claude as author or co-author.
- Work happens in the worktree `.claude/worktrees/server-provisioned-plugins` on branch `feat/server-provisioned-plugins`.

---

### Task 1: Materialize once, per runtime, with no cache

**Files:**
- Modify: `runtime/src/plugins_fetch.rs` (whole file)
- Modify: `models/src/lib.rs:219-221` (delete `ENV_PLUGINS_CACHE`)
- Modify: `runtime-vendor/src/vendor.rs:175-183` (`BundleDelivery`), `:806-822` (env assembly)
- Modify: `cli/src/connect.rs:326-337` (drop `cache_dir` from the literal)

**Interfaces:**
- Consumes: `horsie_models::{ENV_PLUGIN_MANIFEST, ENV_PLUGINS_BASE, ENV_PLUGINS_DIR, ENV_PLUGINS_TOKEN}`.
- Produces: `RuntimeVendor::plugins_root(&self) -> Option<&Path>` and `RuntimeVendor::plugins_path(&self, runtime_id: &str) -> Option<PathBuf>` (both private, used by Task 2); `RuntimeVendor::bundle_env(&self, runtime_id: &str) -> Vec<EnvVar>` (private, unit-tested); `BundleDelivery { base_url: String, dir: String }`.

- [ ] **Step 1: Write the failing tests in `runtime/src/plugins_fetch.rs`**

Replace the existing `provision_fetches_verifies_and_unpacks`, `provision_rejects_hash_mismatch` and `empty_manifest_is_noop` tests with these, and keep `unpack_writes_tree_and_sha_is_stable`, `the_fetch_url_is_built_from_the_base_and_hash` and the `make_zip` / `serve_once` helpers as they are.

```rust
    #[tokio::test]
    async fn provision_fetches_verifies_unpacks_and_marks() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        let base = serve_once(bytes);
        let manifest = serde_json::json!([{ "name": "demo", "hash": hash }]).to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &base, &dir, Some("tok")).await;
        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("demo/skills/a/SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join(MARKER)).unwrap(),
            manifest,
            "a complete materialization records what it was built from"
        );
    }

    /// The whole point of the marker: a respawned runtime must not re-fetch.
    /// The base points at a port nothing listens on, so any fetch attempt would
    /// fail — and the sentinel file proves the directory was never cleared.
    #[tokio::test]
    async fn a_matching_marker_skips_the_fetch_entirely() {
        let manifest = serde_json::json!([{ "name": "demo", "hash": "abc" }]).to_string();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(dir.join("demo/sentinel"), b"kept").unwrap();
        std::fs::write(dir.join(MARKER), &manifest).unwrap();

        let out = provision_into(&manifest, "http://127.0.0.1:1", &dir, None).await;
        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("demo/sentinel").is_file());
    }

    /// A different selection is not a merge: what the last manifest left has to
    /// go, or the session scans skills it did not select.
    #[tokio::test]
    async fn a_changed_manifest_clears_the_directory() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        let base = serve_once(bytes);
        let manifest = serde_json::json!([{ "name": "demo", "hash": hash }]).to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        std::fs::write(dir.join("stale/SKILL.md"), b"old").unwrap();
        std::fs::write(dir.join(MARKER), b"[{\"name\":\"stale\",\"hash\":\"old\"}]").unwrap();

        provision_into(&manifest, &base, &dir, None).await;
        assert!(!dir.join("stale").exists(), "the previous selection is gone");
        assert!(dir.join("demo/skills/a/SKILL.md").is_file());
    }

    /// No marker after a partial run, so the next start retries. Today a bundle
    /// that fails once is lost for the life of the session.
    #[tokio::test]
    async fn a_partial_materialization_writes_no_marker() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        // The stub serves exactly one request, so the second ref cannot be
        // fetched at all.
        let base = serve_once(bytes);
        let manifest = serde_json::json!([
            { "name": "first", "hash": hash },
            { "name": "second", "hash": "deadbeef" },
        ])
        .to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &base, &dir, None).await;

        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("first/skills/a/SKILL.md").is_file());
        assert!(!dir.join("second").exists());
        assert!(!dir.join(MARKER).exists(), "a partial run must be retried");
    }

    #[tokio::test]
    async fn provision_rejects_hash_mismatch() {
        let base = serve_once(make_zip());
        let manifest = serde_json::json!([{ "name": "demo", "hash": "deadbeef" }]).to_string();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        provision_into(&manifest, &base, &dir, None).await;
        assert!(!dir.join("demo").exists());
        assert!(!dir.join(MARKER).exists());
    }

    #[tokio::test]
    async fn empty_manifest_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let out = provision_into("[]", "http://unused", &tmp.path().join("p"), None).await;
        assert!(out.is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-runtime --lib plugins_fetch`
Expected: FAIL — `provision_into` takes five arguments, and `MARKER` does not exist.

- [ ] **Step 3: Rewrite the production half of `runtime/src/plugins_fetch.rs`**

Change the module doc comment's second paragraph, the imports, `provision_plugins`, `provision_into`, `materialize`, and delete `copy_dir`. Everything else in the file stays.

```rust
//! Fetch the session's selected plugin bundles at startup and unpack them into
//! a plugins dir the existing scanner reads. The server injects a manifest of
//! `{name, hash}` refs plus a bearer token via env, and the vendor agent adds
//! the base URL its runtimes can reach the server at plus the directory to
//! unpack into; the runtime GETs each zip over its own outbound connection,
//! verifies the content hash, and materializes the tree.
//!
//! Materialization happens once per runtime, not once per process. The
//! directory records the manifest it was built from, and a start that finds a
//! matching record does nothing — which is what makes a hibernated runtime's
//! respawn free, and keeps it from presenting an artifact token that has since
//! expired.
//!
//! Fully best-effort: any failure is logged and skipped, so a session never
//! fails to start because a bundle was unavailable — it just runs without it.

use horsie_models::{ENV_PLUGIN_MANIFEST, ENV_PLUGINS_BASE, ENV_PLUGINS_DIR, ENV_PLUGINS_TOKEN};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Records the manifest a plugins dir was last materialized from. A plain file,
/// so `plugin_dirs` — which keeps only entries that are directories — never
/// mistakes it for a plugin.
const MARKER: &str = ".manifest.json";
```

`ArtifactRef` and its `url` method are unchanged. Then:

```rust
/// Read the plugin manifest from the environment and materialize the bundles.
/// Returns the plugins dir whenever the manifest named anything, whether or not
/// every bundle landed: there is no host library left to fall back to, so the
/// distinction no longer decides anything.
pub async fn provision_plugins() -> Option<PathBuf> {
    let manifest = std::env::var(ENV_PLUGIN_MANIFEST).ok()?;
    let dir = PathBuf::from(std::env::var(ENV_PLUGINS_DIR).ok()?);
    let base = std::env::var(ENV_PLUGINS_BASE).ok()?;
    let token = std::env::var(ENV_PLUGINS_TOKEN).ok();
    provision_into(&manifest, &base, &dir, token.as_deref()).await
}

/// Env-free core (so tests need not touch process env): parse the manifest,
/// then fetch/verify/unpack each bundle into `dir`.
async fn provision_into(
    manifest: &str,
    base: &str,
    dir: &Path,
    token: Option<&str>,
) -> Option<PathBuf> {
    let refs: Vec<ArtifactRef> = match serde_json::from_str(manifest) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("plugins: ignoring malformed manifest: {e}");
            return None;
        }
    };
    if refs.is_empty() {
        return None;
    }
    if already_materialized(dir, manifest) {
        return Some(dir.to_path_buf());
    }
    // Whatever a previous manifest left behind is not what this session
    // selected, and the scanner reads the whole directory.
    let _ = std::fs::remove_dir_all(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("plugins: cannot create plugins dir {}: {e}", dir.display());
        return None;
    }
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("plugins: http client init failed: {e}");
            return None;
        }
    };
    let mut complete = true;
    for r in &refs {
        if let Err(e) = materialize(&client, r, base, dir, token).await {
            eprintln!("plugins: skipping bundle '{}': {e}", r.name);
            complete = false;
        }
    }
    // Only a complete run earns the marker. A partial one must be retried on
    // the next start rather than frozen in place for the session's whole life.
    if complete {
        if let Err(e) = std::fs::write(dir.join(MARKER), manifest) {
            eprintln!("plugins: cannot record the manifest: {e}");
        }
    }
    Some(dir.to_path_buf())
}

/// Whether `dir` was already built from exactly this manifest.
fn already_materialized(dir: &Path, manifest: &str) -> bool {
    std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|recorded| recorded == manifest)
}

async fn materialize(
    client: &reqwest::Client,
    r: &ArtifactRef,
    base: &str,
    dir: &Path,
    token: Option<&str>,
) -> Result<(), String> {
    let mut req = client.get(r.url(base));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let got = sha256_hex(&bytes);
    if got != r.hash {
        return Err(format!("hash mismatch (want {}, got {got})", r.hash));
    }
    unpack_zip(&bytes, &dir.join(&r.name))
}
```

Delete the `copy_dir` function entirely.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-runtime --lib plugins_fetch`
Expected: PASS, 7 tests.

- [ ] **Step 5: Delete `ENV_PLUGINS_CACHE` from `models/src/lib.rs`**

Remove these three lines (currently 219-221):

```rust
/// Optional content-hash cache dir (local vendor) so repeated sessions avoid
/// re-fetching and re-unpacking identical bundles.
pub const ENV_PLUGINS_CACHE: &str = "HORSIE_PLUGINS_CACHE";
```

And retarget the `ENV_PLUGINS_DIR` doc comment, which is now a per-runtime path:

```rust
/// Directory the runtime unpacks fetched bundles into and scans as its
/// plugins_dir. One per runtime: the runtime scans the whole directory, so a
/// shared one would show a session another session's skills.
pub const ENV_PLUGINS_DIR: &str = "HORSIE_PLUGINS_DIR";
```

- [ ] **Step 6: Write the failing vendor test**

Add to `mod tests` in `runtime-vendor/src/vendor.rs`:

```rust
    /// A shared bundles directory would let one session scan another's skills,
    /// because the runtime scans the whole directory it is pointed at.
    #[test]
    fn the_bundle_env_names_a_directory_per_runtime() {
        let agent = agent().with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: "/state/plugins".to_string(),
        });
        let value = |vars: &[EnvVar], name: &str| {
            vars.iter()
                .find(|v| v.name == name)
                .map(|v| v.value.clone())
        };

        let one = agent.bundle_env("rt-1");
        let two = agent.bundle_env("rt-2");

        assert_eq!(
            value(&one, horsie_models::ENV_PLUGINS_DIR).as_deref(),
            Some("/state/plugins/rt-1")
        );
        assert_eq!(
            value(&two, horsie_models::ENV_PLUGINS_DIR).as_deref(),
            Some("/state/plugins/rt-2")
        );
        assert_eq!(
            value(&one, horsie_models::ENV_PLUGINS_BASE).as_deref(),
            Some("http://127.0.0.1:3789")
        );
    }

    #[test]
    fn an_agent_serving_no_bundles_adds_no_bundle_env() {
        assert!(agent().bundle_env("rt-1").is_empty());
    }
```

- [ ] **Step 7: Run it to verify it fails**

Run: `cargo test -p horsie-runtime-vendor --lib bundle_env`
Expected: FAIL — `bundle_env` does not exist, and `BundleDelivery` still has a `cache_dir` field.

- [ ] **Step 8: Change `BundleDelivery` and extract `bundle_env`**

In `runtime-vendor/src/vendor.rs`, replace the `BundleDelivery` struct:

```rust
/// Where and how an agent's runtimes materialize server-managed bundles.
pub struct BundleDelivery {
    /// Base URL reaching the server *from where the runtimes run* — loopback
    /// for a local agent, an advertise address for a remote one.
    pub base_url: String,
    /// Root under which each runtime gets its own directory to unpack into.
    pub dir: String,
}
```

Update the `bundles` field's doc comment on `RuntimeVendor` to drop the cache:

```rust
    /// How this agent's runtimes fetch server-managed bundles: the base URL
    /// that reaches the server from where they run, and the root they unpack
    /// into. Both are the agent's knowledge, not the server's — it sends only
    /// hashes and a token.
    bundles: Option<BundleDelivery>,
```

Widen the path import at `runtime-vendor/src/vendor.rs:35`, which currently brings in `PathBuf` only:

```rust
use std::path::{Path, PathBuf};
```

Add three private helpers to the `impl RuntimeVendor` block, next to `spec_path`:

```rust
    /// Root under which each runtime materializes its session's bundles.
    /// `None` when this agent serves none.
    fn plugins_root(&self) -> Option<&Path> {
        self.bundles.as_ref().map(|b| Path::new(b.dir.as_str()))
    }

    /// Where `runtime_id` materializes its session's bundles. One directory per
    /// runtime: the runtime scans the whole directory it is given, so a shared
    /// one would show a session every other session's skills.
    fn plugins_path(&self, runtime_id: &str) -> Option<PathBuf> {
        self.plugins_root().map(|root| root.join(runtime_id))
    }

    /// The bundle-delivery environment for one runtime. Extracted from
    /// `provision` so the per-runtime path is testable without spawning
    /// anything.
    fn bundle_env(&self, runtime_id: &str) -> Vec<EnvVar> {
        let Some(b) = &self.bundles else {
            return Vec::new();
        };
        let mut env = vec![EnvVar {
            name: horsie_models::ENV_PLUGINS_BASE.to_string(),
            value: b.base_url.clone(),
        }];
        if let Some(dir) = self.plugins_path(runtime_id) {
            env.push(EnvVar {
                name: horsie_models::ENV_PLUGINS_DIR.to_string(),
                value: dir.to_string_lossy().into_owned(),
            });
        }
        env
    }
```

Then in `provision`, replace the whole `if let Some(b) = &self.bundles { ... }` block (currently lines 806-822) with:

```rust
        let mut env = request.env.clone();
        env.extend(self.bundle_env(runtime_id));
```

- [ ] **Step 9: Drop `cache_dir` from the CLI's literal**

In `cli/src/connect.rs`, the `.with_bundles(...)` call becomes:

```rust
    .with_bundles(horsie_runtime_vendor::BundleDelivery {
        // The runtimes run on this machine, so whatever address reaches the
        // server from here reaches it from them.
        base_url: server.trim_end_matches('/').to_string(),
        dir: state_dir.join("plugins").to_string_lossy().into_owned(),
    });
```

- [ ] **Step 10: Run the tests to verify they pass**

Run: `cargo test -p horsie-runtime-vendor --lib && cargo test -p horsie-runtime --lib && cargo build -p horsie`
Expected: PASS, and the CLI builds.

- [ ] **Step 11: Commit**

```bash
git add runtime/src/plugins_fetch.rs models/src/lib.rs runtime-vendor/src/vendor.rs cli/src/connect.rs
git commit -m "plugins: one directory per runtime, materialized once"
```

---

### Task 2: Delete a runtime's bundles with the runtime

**Files:**
- Modify: `runtime-vendor/src/vendor.rs` — `DeleteRuntime` arm (currently `:685-697`), `run` (`:386`), plus a new `sweep_plugin_dirs`

**Interfaces:**
- Consumes: `plugins_path`, `plugins_root`, `spec_path` from Task 1.
- Produces: `RuntimeVendor::sweep_plugin_dirs(&self)` (private, unit-tested).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `runtime-vendor/src/vendor.rs`:

```rust
    /// Boot is the one moment when "no runtime process is live" is guaranteed,
    /// so the only question left is whether the runtime still exists at all —
    /// and the spec file is the same record that decides whether it could be
    /// revived.
    #[test]
    fn boot_sweeps_bundle_dirs_with_no_surviving_spec() {
        let state = tempfile::tempdir().expect("tempdir");
        let plugins = state.path().join("plugins");
        std::fs::create_dir_all(plugins.join("kept/demo")).expect("mkdir");
        std::fs::create_dir_all(plugins.join("orphan/demo")).expect("mkdir");
        // `kept` is revivable: it has a persisted spec.
        std::fs::create_dir_all(state.path().join("kept")).expect("mkdir");
        std::fs::write(state.path().join("kept/spec.json"), b"{}").expect("write");

        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        )
        .with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: plugins.to_string_lossy().into_owned(),
        });

        agent.sweep_plugin_dirs();

        assert!(plugins.join("kept/demo").is_dir(), "a revivable runtime keeps its bundles");
        assert!(!plugins.join("orphan").exists(), "crash debris is removed");
    }

    /// Deleting a session takes its bundles; stopping a process must not.
    /// A hibernated runtime has to find them still there when it wakes, which
    /// is the whole reason materialization can happen once.
    #[test]
    fn forgetting_a_runtime_removes_both_its_dirs() {
        let state = tempfile::tempdir().expect("tempdir");
        let plugins = state.path().join("plugins");
        std::fs::create_dir_all(plugins.join("rt-1/demo")).expect("mkdir");
        std::fs::create_dir_all(state.path().join("rt-1")).expect("mkdir");
        std::fs::write(state.path().join("rt-1/spec.json"), b"{}").expect("write");

        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        )
        .with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: plugins.to_string_lossy().into_owned(),
        });

        agent.forget_runtime_dirs("rt-1");

        assert!(!state.path().join("rt-1").exists(), "the rebuild record is gone");
        assert!(!plugins.join("rt-1").exists(), "and so are its bundles");
    }

    /// An agent that serves no bundles has no root to sweep, and must not
    /// wander into the state dir looking for one.
    #[test]
    fn sweeping_without_bundles_is_a_noop() {
        let state = tempfile::tempdir().expect("tempdir");
        std::fs::write(state.path().join("keep-me"), b"x").expect("write");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        );
        agent.sweep_plugin_dirs();
        assert!(state.path().join("keep-me").is_file());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p horsie-runtime-vendor --lib sweep`
Expected: FAIL — `sweep_plugin_dirs` does not exist.

- [ ] **Step 3: Add `sweep_plugin_dirs`**

Add to the `impl RuntimeVendor` block, next to `plugins_path`:

```rust
    /// Remove bundle directories belonging to runtimes this agent can no longer
    /// revive.
    ///
    /// Called once at startup, where no runtime process is live by definition,
    /// so anything without a persisted spec is crash debris. A vendor that
    /// persists no specs at all cannot revive anything, and correctly loses
    /// every directory here.
    ///
    /// Best-effort throughout: an unreadable root or an undeletable directory
    /// costs disk, and is not worth refusing to start over.
    fn sweep_plugin_dirs(&self) {
        let Some(root) = self.plugins_root() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(runtime_id) = name.to_str() else {
                continue;
            };
            if self.spec_path(runtime_id).is_file() {
                continue;
            }
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-runtime-vendor --lib sweep`
Expected: PASS, 2 tests.

- [ ] **Step 5: Add `forget_runtime_dirs`, call the sweep at boot, and delete on `DeleteRuntime`**

Add next to `sweep_plugin_dirs`:

```rust
    /// Drop everything on disk belonging to a runtime whose session is gone:
    /// the record of how to rebuild it, and the bundles it materialized.
    ///
    /// Deliberately not called from `halt`. Stopping a process is not losing a
    /// session, and a hibernated runtime must find its bundles still there when
    /// it wakes — that is what makes materialization a once-per-runtime cost.
    fn forget_runtime_dirs(&self, runtime_id: &str) {
        let _ = std::fs::remove_dir_all(self.state_dir.join(runtime_id));
        if let Some(dir) = self.plugins_path(runtime_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
```

In `run`, immediately after the `client_request(server_url, None).map_err(AgentExit::Fatal)?;` line:

```rust
        // Nothing this agent owns is running yet, so any bundle directory
        // without a spec belongs to a runtime that cannot come back.
        self.sweep_plugin_dirs();
```

In the `RuntimeVendorCommand::DeleteRuntime` arm, replace the existing `let _ = std::fs::remove_dir_all(self.state_dir.join(&cmd.runtime_id));` line and the comment above it with:

```rust
                // The session is gone, so the record of how to rebuild its
                // runtime goes with it, and so do its bundles — otherwise a
                // deleted session's state would outlive it on disk forever.
                self.forget_runtime_dirs(&cmd.runtime_id);
```

- [ ] **Step 6: Verify the crate still builds and tests pass**

Run: `cargo test -p horsie-runtime-vendor --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add runtime-vendor/src/vendor.rs
git commit -m "plugins: a runtime's bundles die with the runtime, not with its process"
```

---

### Task 3: Delete the CLI plugin and marketplace surface, and the host library

One commit: the CLI's `library_for_runtime` and the vendor's `with_host_library` are each other's only caller, so removing either alone does not compile.

**Files:**
- Delete: `cli/src/plugins.rs`, `cli/src/marketplace.rs`
- Modify: `cli/src/lib.rs`, `cli/src/main.rs`, `cli/src/config.rs`, `cli/src/connect.rs`
- Modify: `runtime-vendor/src/vendor.rs`, `runtime-vendor/src/process_provider.rs`
- Modify: `support/src/plugin/grants.rs` (delete `plugin_library_grants`), `support/src/plugin/mod.rs` (drop the `grants` module if it becomes empty)
- Modify: `models/fluorite/executor.fl`, `runtime/src/main.rs`, `runtime/tests/provision_steps.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `RuntimeVendor::with_hook_path(self, hook_path: Vec<PathBuf>) -> Self`; `horsie::connect::resolve_hook_path(configured: Option<Vec<PathBuf>>) -> Vec<PathBuf>`; `connect::run(runtime_bin: &Path, server: &str, workspaces: &[String], vendor_name: &str, background: bool, state_dir: &Path, hook_path: Vec<PathBuf>, sandbox: bool)`.

- [ ] **Step 1: Delete the two CLI modules and their registrations**

```bash
git rm cli/src/plugins.rs cli/src/marketplace.rs
```

In `cli/src/lib.rs`, delete the `pub mod marketplace;` and `pub mod plugins;` lines.

- [ ] **Step 2: Move `resolve_hook_path` and `which_dir` into `cli/src/connect.rs`**

Add near the top of `cli/src/connect.rs`, after the imports (`std::process::Command` needs importing if it is not already):

```rust
/// Resolve the hook interpreter dirs: the configured override, else
/// auto-discover `node` from the ambient environment (its parent dir). Empty
/// when neither resolves.
///
/// Resolved unconditionally. It used to be gated on a populated host plugin
/// library, which meant a user whose skills all came from the server got no
/// interpreter and none of their bundles' hooks could run.
pub fn resolve_hook_path(configured: Option<Vec<PathBuf>>) -> Vec<PathBuf> {
    if let Some(paths) = configured {
        return paths;
    }
    which_dir("node").into_iter().collect()
}

/// The directory containing `bin` on the current `PATH`, via `command -v`.
fn which_dir(bin: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    PathBuf::from(path).parent().map(Path::to_path_buf)
}
```

- [ ] **Step 3: Strip the host library out of `cli/src/connect.rs`**

Delete `plugins_summary` (`:111-114`) and `struct PluginLibrary` (`:116-123`).

In `run`, change the `plugins: Option<PluginLibrary>` parameter to `hook_path: Vec<PathBuf>`, delete the `if let Some(p) = &plugins { agent = agent.with_host_library(...) }` block and the `if let Some(p) = &plugins { println!(plugins_summary(...)) }` block, and chain the hook path onto the builder:

```rust
    .with_bundles(horsie_runtime_vendor::BundleDelivery {
        base_url: server.trim_end_matches('/').to_string(),
        dir: state_dir.join("plugins").to_string_lossy().into_owned(),
    })
    .with_hook_path(hook_path);
```

Note the `let mut agent` binding can become `let agent` once nothing reassigns it.

- [ ] **Step 4: Strip the command trees from `cli/src/main.rs`**

Delete:
- the `Marketplace` and `Plugin` variants of `enum Command` (`:33-42`);
- `enum MarketplaceAction` (`:323-364`) and `enum PluginAction` (`:366-402`);
- `resolve_plugin_paths` (`:404-412`);
- the `Command::Marketplace` and `Command::Plugin` match arms (`:416-545`).

Replace the plugin wiring in the `Command::Connect` arm with a hook-path resolution:

```rust
            let hook_path = connect::resolve_hook_path(cfg.runtime.hook_path.clone());
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &name,
                background,
                &cfg.storage.state_dir,
                hook_path,
                !no_sandbox,
            )
            .await
```

Add a one-line notice above the `connect::run` call, so a user with an old library learns it is inert:

```rust
            println!(
                "note: skills now come from the server per session; a local \
                 plugin library is no longer read and can be deleted"
            );
```

- [ ] **Step 5: Strip the dead storage config**

In `cli/src/config.rs`, `StorageConfig` keeps only `state_dir`:

```rust
#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Ephemeral runtime state: the shared local-runtime-vendor socket, the
    /// per-runtime scratch dirs, and the bundles each runtime materializes.
    /// Defaults to `$XDG_STATE_HOME/horsie`, else `$HOME/.local/state/horsie`
    /// (same path on macOS and Linux).
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
        }
    }
}
```

Delete `default_data_dir`, `default_plugins_dir` and `storage_dir_from`, and inline the one surviving case:

```rust
/// Default state dir: `$XDG_STATE_HOME/horsie` if set, else
/// `$HOME/.local/state/horsie`. Same path on macOS and Linux. With neither env
/// var (rare), a relative `./.horsie/state`.
fn default_state_dir() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x).join("horsie"),
        _ => match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h).join(".local/state").join("horsie"),
            _ => PathBuf::from("./.horsie").join("state"),
        },
    }
}
```

Then fix the config tests: delete any assertion naming `data_dir` or `plugins_dir` (around `:313-333`), and any test of `storage_dir_from`. Keep the test that a `"storage": { "state_dir": "/s" }` file parses.

- [ ] **Step 6: Remove the host library from the vendor**

In `runtime-vendor/src/vendor.rs`:
- delete the `host_library` and `host_sources` fields and their initializers in `new`;
- delete `with_host_library`;
- add, next to `with_bundles`:

```rust
    /// Directories prepended to PATH when a runtime runs plugin hooks, and
    /// granted read access in the sandbox.
    #[must_use]
    pub fn with_hook_path(mut self, hook_path: Vec<PathBuf>) -> Self {
        self.hook_path = hook_path;
        self
    }
```

- in `provision`, `RuntimeConfig` loses its `plugins_dir` field (see Step 8);
- in `write_caps_file`, delete the `plugin_library_grants` call and the `sources` local. Task 4 puts the replacement grants here; for this commit the function is the baseline plus the respawnable state-dir grant. Update its doc comment to say so.

Delete the now-stale test `the_written_caps_file_grants_the_host_plugin_library_and_its_sources`. Keep `the_written_caps_file_is_the_baseline_without_a_host_library`.

- [ ] **Step 7: Delete `plugin_library_grants`**

Delete `support/src/plugin/grants.rs` and the `pub mod grants;` line at `support/src/plugin/mod.rs:10`. There is no `pub use` re-export of it to remove — the module is reached by path.

```bash
git rm support/src/plugin/grants.rs
```

- [ ] **Step 8: Delete the dead `plugins_dir` wire**

In `models/fluorite/executor.fl`, delete the `plugins_dir: Option<String>,` field from `RuntimeConfig` (`:37`) and its doc comment, then:

```bash
touch models/build.rs
```

In `runtime-vendor/src/process_provider.rs`, delete the `--plugins-dir` argument block (`:69-72`), the `plugins_dir: None` initializer in its test fixture (`:234`), and the test that asserts the flag is passed (`:275`, `:292` — delete the whole test function containing them).

In `runtime/src/main.rs`, delete the `--plugins-dir` clap field (`:58-59`) and simplify the resolution (`:228-233`):

```rust
    // The session's selected bundles, fetched by this runtime over its own
    // outbound connection. The only source of skills there is.
    let plugins_dir = horsie_runtime::plugins_fetch::provision_plugins().await;
    let registry = Arc::new(
        horsie_runtime::workspace::WorkspaceRegistry::new(cli.workspaces)
            .with_plugins(plugins_dir, cli.hook_path),
    );
```

In `runtime/tests/provision_steps.rs:102`, delete the `plugins_dir: None,` line from the `RuntimeConfig` literal.

- [ ] **Step 9: Build and test everything touched**

Run: `cargo build --workspace --all-targets 2>&1 | tail -40`
Expected: clean. Fix any remaining reference the deletions exposed — likely candidates are `cli/tests/connect_e2e.rs` (it may call `connect::run`) and further `RuntimeConfig` literals.

Run: `cargo test -p horsie --lib && cargo test -p horsie-runtime-vendor --lib && cargo test -p horsie-runtime --lib && cargo test -p horsie-support --lib`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "cli: the server owns skills; drop plugin and marketplace management"
```

---

### Task 4: Grant the sandbox the directory it unpacks into

Without this a sandboxed `horsie connect` — the default — cannot write its bundles, and because provisioning is best-effort it fails to "no skills" silently.

**Files:**
- Create: `support/src/plugin/grants.rs` (new contents)
- Modify: `support/src/plugin/mod.rs`, `runtime-vendor/src/vendor.rs` (`write_caps_file`)

**Interfaces:**
- Consumes: `RuntimeVendor::plugins_path` (Task 1), `RuntimeVendor::with_hook_path` (Task 3).
- Produces: `horsie_support::plugin::grants::session_plugin_grants(plugins_dir: Option<&Path>, hook_path: &[PathBuf]) -> Vec<Grant>`.

- [ ] **Step 1: Write the failing grants test**

Create `support/src/plugin/grants.rs`:

```rust
//! Sandbox grants for the bundles a runtime materializes for its session.

use horsie_models::capabilities::{Access, DirGrant, Grant};
use std::path::{Path, PathBuf};

/// Grants a sandboxed runtime needs to provision and read its session's
/// bundles: read-write on the directory it unpacks into — the sandbox is
/// applied before the fetch runs, so a read grant is not enough — and read on
/// the hook interpreter dirs.
pub fn session_plugin_grants(plugins_dir: Option<&Path>, hook_path: &[PathBuf]) -> Vec<Grant> {
    let mut out = Vec::new();
    if let Some(dir) = plugins_dir {
        out.push(Grant::Dir(DirGrant {
            path: dir.to_string_lossy().into_owned(),
            access: Access::ReadWrite,
        }));
    }
    out.extend(hook_path.iter().map(|p| {
        Grant::Dir(DirGrant {
            path: p.to_string_lossy().into_owned(),
            access: Access::Read,
        })
    }));
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn the_plugins_dir_is_writable_because_the_runtime_unpacks_into_it() {
        let g = session_plugin_grants(Some(Path::new("/state/plugins/rt-1")), &[]);
        assert_eq!(g.len(), 1);
        assert!(
            matches!(&g[0], Grant::Dir(d)
                if d.path == "/state/plugins/rt-1" && d.access == Access::ReadWrite),
            "got {:?}",
            g[0]
        );
    }

    #[test]
    fn hook_dirs_are_read_only_and_granted_without_a_plugins_dir() {
        let g = session_plugin_grants(None, &[PathBuf::from("/opt/node/bin")]);
        assert!(
            matches!(&g[..], [Grant::Dir(d)]
                if d.path == "/opt/node/bin" && d.access == Access::Read),
            "got {g:?}"
        );
    }

    #[test]
    fn nothing_to_grant_yields_nothing() {
        assert!(session_plugin_grants(None, &[]).is_empty());
    }
}
```

Re-add `pub mod grants;` to `support/src/plugin/mod.rs`, in alphabetical position among the sibling `pub mod` lines.

- [ ] **Step 2: Run it to verify it passes on its own**

Run: `cargo test -p horsie-support --lib grants`
Expected: PASS, 3 tests.

- [ ] **Step 3: Write the failing vendor test**

Add to `mod tests` in `runtime-vendor/src/vendor.rs`:

```rust
    /// The sandbox is applied before the runtime fetches its bundles, so the
    /// directory it unpacks into has to be writable — a read grant leaves the
    /// unpack failing, and provisioning is best-effort, so it fails silently to
    /// "no skills".
    #[test]
    fn the_written_caps_file_grants_the_runtimes_own_plugins_dir_and_hook_path() {
        let state = tempfile::tempdir().expect("tempdir");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        )
        .with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: "/state/plugins".to_string(),
        })
        .with_hook_path(vec![PathBuf::from("/opt/node/bin")]);

        let path = agent.write_caps_file("rt-1").expect("write caps");
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read caps")).expect("parse caps");
        // The grant union is fluorite-tagged: `{"type":"Dir","value":{"path":…}}`.
        let grant = |path: &str| {
            written["grants"]
                .as_array()
                .expect("grants array")
                .iter()
                .find(|g| {
                    g.get("value").and_then(|v| v.get("path")).and_then(serde_json::Value::as_str)
                        == Some(path)
                })
                .cloned()
        };

        let plugins = grant("/state/plugins/rt-1").expect("the runtime's own plugins dir");
        assert_eq!(plugins["value"]["access"], "ReadWrite");
        let hooks = grant("/opt/node/bin").expect("the hook interpreter dir");
        assert_eq!(hooks["value"]["access"], "Read");
        // Another runtime's directory is not granted.
        assert!(grant("/state/plugins/rt-2").is_none());
        // The baseline's own grants survive alongside them.
        assert!(grant("/usr").is_some());
    }
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test -p horsie-runtime-vendor --lib the_written_caps_file_grants_the_runtimes_own`
Expected: FAIL — the plugins dir is not in the written grants.

- [ ] **Step 5: Wire the grants into `write_caps_file`**

In `runtime-vendor/src/vendor.rs`, after `let mut spec = crate::baseline::baseline_capabilities()?;`:

```rust
        spec.grants
            .extend(horsie_support::plugin::grants::session_plugin_grants(
                self.plugins_path(runtime_id).as_deref(),
                &self.hook_path,
            ));
```

Update the function's doc comment:

```rust
    /// Persist the effective capability spec for a runtime and return its path.
    ///
    /// The spec is this vendor's [`baseline`](crate::baseline) plus the
    /// directory this runtime materializes its session's bundles into and the
    /// hook interpreter dirs. The bundles dir is read-write: the sandbox is
    /// applied before the runtime fetches, so it unpacks under confinement.
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p horsie-runtime-vendor --lib && cargo test -p horsie-support --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add support runtime-vendor/src/vendor.rs
git commit -m "sandbox: grant a runtime the directory it unpacks its bundles into"
```

---

### Task 5: Update the guides

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`, `docs/guide/README.md:12`, `docs/guide/runtime-vendors.md:105`, `docs/guide/settings-reference.md:51-56`

- [ ] **Step 1: Rewrite the CLI-install parts of `docs/guide/skills-and-plugins.md`**

Read the whole file first — the edits below are located by line number against the current revision, and the surrounding prose decides how much of each section survives.

The framing rule for public docs: the product is "horsie server", and skills are managed there. Specifically:

- The numbered list at `:82-83` currently starts with "Install plugins with the CLI: `horsie plugin install <git-url>`". Replace that step with adding the plugin in **Settings → Skills** in the web UI, which is where marketplaces and bundles already live since #210.
- The marketplace section at `:170-183` shows `horsie marketplace add|show|list` and `horsie plugin install x@marketplace`. Replace the command block with the equivalent web UI flow, keeping the surrounding prose about what a marketplace is.
- The clone-sharing paragraph at `:197-199` describes `horsie plugin update` and `horsie plugin remove` against `sources/`. That mechanism is now server-side; rewrite it to describe the server's shared clones, or delete it if the server's behaviour is already documented elsewhere in the file.
- Add a short paragraph stating that a session's skills are the bundles selected for it, that the runtime fetches them at startup into a directory of its own, and that they are deleted when the session is.

- [ ] **Step 2: Fix the two one-line references**

`docs/guide/README.md:12` — "`horsie session tail` streams session events, and `horsie plugin` manages the …". Drop the `horsie plugin` clause.

`docs/guide/runtime-vendors.md:105` — the sentence pointing at `horsie plugin install`. Point at the web UI's Skills settings instead.

- [ ] **Step 3: Tighten the settings note**

`docs/guide/settings-reference.md:54-56` already says old files setting `storage.plugins_dir` or `runtime.hook_path` keep parsing because the keys are ignored. `runtime.hook_path` is **not** ignored — the CLI still reads it. Correct that sentence: `storage.plugins_dir` and `storage.data_dir` are ignored by both; `runtime.hook_path` is CLI-only and still honoured.

- [ ] **Step 4: Check nothing else references the removed commands**

Run: `grep -rn "horsie plugin \|horsie marketplace \|storage.plugins_dir\|plugins_dir" docs/guide README.md CLAUDE.md`
Expected: no hits outside `settings-reference.md`'s deliberate "these keys are ignored" note and the server's own `ServerInfo.plugins_dir` (the artifact store, unrelated).

- [ ] **Step 5: Commit**

```bash
git add docs/guide
git commit -m "docs: skills come from the server, not the CLI"
```

---

### Task 6: Full gate and PR

- [ ] **Step 1: Run the whole gate once**

Run: `make check`
Expected: PASS. `make check` is `fmt-check` + `clippy` + `test`; run `cargo fmt` first if `fmt-check` fails, then re-run.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin feat/server-provisioned-plugins
```

PR body: one short paragraph on why (two competing plugin libraries, and the wrong one won by accident), one on what changed, and a line linking #242, #243 and #244 as the deliberate follow-ups. No test-by-test narration, no CI status, no diff restatement. One long line per paragraph — do not hard-wrap.

- [ ] **Step 3: Verify the sandboxed path by hand**

CI cannot run a sandboxed `horsie connect`. Against a local server with a skill bundle selected for a session:

```bash
cargo build -p horsie -p horsie-runtime
./target/debug/horsie connect --server http://127.0.0.1:3789 --workspace main=/tmp/ws
```

Confirm `<state_dir>/plugins/<session-id>/` contains the unpacked bundle and a `.manifest.json`, that the session's agent can see the skill, that stopping and resuming the session does not re-download (the directory's mtime is unchanged), and that deleting the session removes the directory. Record the result in the PR.
