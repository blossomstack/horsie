# Local Runtime Host Plugins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `horsie connect` passes the CLI-installed host plugin library (`--plugins-dir`/`--hook-path`) to the `horsie-runtime` it spawns, so sessions on the local vendor see skills exactly like the daemon/job path.

**Architecture:** A shared helper in `cli/src/plugins.rs` resolves the library (populated plugins dir + hook path), used by both the daemon and `horsie connect`. `connect.rs` gains a pure, unit-tested argv builder that appends the flags. No runtime, server, or protocol changes — the server already scans every session vendor-agnostically (`session_actor.rs:785`) and the runtime already prefers env-manifest provisioning with `--plugins-dir` as fallback (`runtime/src/main.rs:149-151`).

**Tech Stack:** Rust (edition 2024), clap, tempfile (dev-dep, already present).

**Spec:** `docs/superpowers/specs/2026-07-26-local-runtime-host-plugins-design.md`

## Global Constraints

- Work in the worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie-connect-plugins`, branch `feat/connect-local-plugins`.
- No AI attribution in any commit message or PR body. Commit messages: short subject line only.
- Repo gate before finishing: `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features` all green.
- Workspace lints are deny-by-default (`[lints] workspace = true`); test modules use `#[allow(clippy::unwrap_used, ...)]` per existing pattern.
- **E2E note (deviation from spec, verified during planning):** no new e2e is needed. The Playwright harness already spawns `horsie-runtime --plugins-dir` against a real server and asserts shared-skill + SessionStart-hook loading end-to-end (`clients/web/e2e/f-context-loading.spec.ts` F2/F3, fixtures in `global-setup.ts:72-90,164`). The only uncovered seam is CLI argv wiring, which is unit-tested here.

---

### Task 1: Shared `library_for_runtime` helper in `cli/src/plugins.rs`

The daemon resolves the plugin library inline (`cli/src/daemon/mod.rs:128-134`). Extract that logic into a reusable helper so `horsie connect` can share it (DRY), and refactor the daemon to use it.

**Files:**
- Modify: `cli/src/plugins.rs` (add helper after `resolve_hook_path`, ~line 67; add tests in `mod tests`, line 252)
- Modify: `cli/src/daemon/mod.rs:127-134` (use the helper)

**Interfaces:**
- Consumes: existing `plugins_dir_if_populated(&Path) -> Option<PathBuf>` and `resolve_hook_path(Option<Vec<PathBuf>>) -> Vec<PathBuf>` in the same file.
- Produces: `pub fn library_for_runtime(plugins_dir: &Path, hook_path: Option<Vec<PathBuf>>) -> (Option<PathBuf>, Vec<PathBuf>)` — used by `daemon/mod.rs` (this task) and `cli/src/main.rs` connect dispatch (Task 2).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `cli/src/plugins.rs` (the module already imports `tempfile::TempDir`):

```rust
    #[test]
    fn library_for_runtime_empty_dir_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let (plugins, hooks) =
            library_for_runtime(dir.path(), Some(vec![PathBuf::from("/opt/node/bin")]));
        assert!(plugins.is_none());
        // No library → hook path not resolved, even with an override configured.
        assert!(hooks.is_empty());
    }

    #[test]
    fn library_for_runtime_populated_resolves_hooks() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sp")).unwrap();
        let (plugins, hooks) =
            library_for_runtime(dir.path(), Some(vec![PathBuf::from("/opt/node/bin")]));
        assert_eq!(plugins, Some(dir.path().to_path_buf()));
        assert_eq!(hooks, vec![PathBuf::from("/opt/node/bin")]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie --lib plugins::tests::library_for_runtime`
Expected: FAIL with "cannot find function `library_for_runtime` in this scope"

- [ ] **Step 3: Implement the helper**

Add to `cli/src/plugins.rs`, immediately after `resolve_hook_path`:

```rust
/// Resolve the shared plugin library for a spawned runtime: the plugins root iff
/// it holds ≥1 plugin, plus the hook interpreter dirs — resolved only when there
/// is a library to run hooks for. Shared by the daemon and `horsie connect`.
pub fn library_for_runtime(
    plugins_dir: &Path,
    hook_path: Option<Vec<PathBuf>>,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let dir = plugins_dir_if_populated(plugins_dir);
    let hooks = if dir.is_some() {
        resolve_hook_path(hook_path)
    } else {
        Vec::new()
    };
    (dir, hooks)
}
```

- [ ] **Step 4: Refactor the daemon to use it**

In `cli/src/daemon/mod.rs`, replace lines 127-134:

```rust
    // Resolve the shared plugin library once: only when the dir holds ≥1 plugin. The
    // hook interpreter dirs (config override else discovered `node`) are resolved only
    // when there is a library to run hooks for.
    let plugins_dir = crate::plugins::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        crate::plugins::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else {
        Vec::new()
    };
```

with:

```rust
    // Resolve the shared plugin library once: only when the dir holds ≥1 plugin. The
    // hook interpreter dirs (config override else discovered `node`) are resolved only
    // when there is a library to run hooks for.
    let (plugins_dir, hook_path) =
        crate::plugins::library_for_runtime(&cfg.storage.plugins_dir, cfg.runtime.hook_path.clone());
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie --lib`
Expected: PASS, including the two new tests and all pre-existing ones

- [ ] **Step 6: Commit**

```bash
git add cli/src/plugins.rs cli/src/daemon/mod.rs
git commit -m "cli: factor shared plugin-library resolution out of the daemon"
```

---

### Task 2: `horsie connect` passes the library to `horsie-runtime`

`connect::run` (`cli/src/connect.rs:65`) builds the `horsie-runtime` command inline and never passes `--plugins-dir`. Factor the argv into a pure function (unit-testable), extend it with the plugins flags, take the library as new `run()` parameters, and wire the dispatch in `main.rs`.

**Files:**
- Modify: `cli/src/connect.rs` (add `runtime_args` + `plugins_summary`, change `run` signature, add tests)
- Modify: `cli/src/main.rs:550-570` (connect dispatch: resolve library, pass to `run`)

**Interfaces:**
- Consumes: `plugins::library_for_runtime` (Task 1); existing `plugins::count_installed(&Path) -> usize`; `cfg.storage.plugins_dir: PathBuf`, `cfg.runtime.hook_path: Option<Vec<PathBuf>>` (already resolved in the dispatch, `main.rs:557`).
- Produces:
  - `pub fn runtime_args(endpoint: &str, runtime_id: &str, workspaces: &[String], plugins_dir: Option<&Path>, hook_path: &[PathBuf]) -> Vec<String>`
  - `pub fn plugins_summary(plugins_dir: &Path, count: usize) -> String`
  - `pub fn run(runtime_bin: &Path, server: &str, workspaces: &[String], runtime_id: &str, background: bool, state_dir: &Path, plugins_dir: Option<PathBuf>, hook_path: Vec<PathBuf>) -> Result<i32, CliError>` (two new trailing params)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `cli/src/connect.rs` (the file's tests already use `super::*`; add `use std::path::PathBuf;` to the test module's imports — `Path` is already imported at file level):

```rust
    #[test]
    fn runtime_args_omit_plugins_flags_without_library() {
        let args = runtime_args(
            "ws://h:3789/api/runtime/connect?register=local",
            "local",
            &["main=.".to_string()],
            None,
            &[],
        );
        assert_eq!(
            args,
            vec![
                "--endpoint",
                "ws://h:3789/api/runtime/connect?register=local",
                "--runtime-id",
                "local",
                "--workspace",
                "main=.",
            ]
        );
    }

    #[test]
    fn runtime_args_append_plugins_dir_and_hook_paths() {
        let args = runtime_args(
            "ws://h/x",
            "local",
            &["main=.".to_string()],
            Some(Path::new("/home/u/.local/share/horsie/plugins")),
            &[PathBuf::from("/opt/node/bin"), PathBuf::from("/usr/local/bin")],
        );
        let tail = &args[args.len() - 6..];
        assert_eq!(
            tail,
            [
                "--plugins-dir",
                "/home/u/.local/share/horsie/plugins",
                "--hook-path",
                "/opt/node/bin",
                "--hook-path",
                "/usr/local/bin",
            ]
        );
    }

    #[test]
    fn plugins_summary_renders_count_and_dir() {
        assert_eq!(
            plugins_summary(Path::new("/p"), 3),
            "plugins: 3 installed from /p"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie --lib connect::tests`
Expected: FAIL with "cannot find function `runtime_args` in this scope"

- [ ] **Step 3: Implement `runtime_args` + `plugins_summary`, rewire `run`**

In `cli/src/connect.rs`, add `PathBuf` to the path import (line 6: `use std::path::{Path, PathBuf};`), then add after `connection_summary` (line 58):

```rust
/// The argv for the spawned `horsie-runtime`, factored out of `run` so the
/// plugins wiring is unit-testable. `workspaces` must already be normalized
/// (`name=path`). `--plugins-dir`/`--hook-path` are appended only when the
/// host library resolved — the runtime then exposes it as the read-only
/// `horsie_shared` workspace and runs plugin SessionStart hooks.
pub fn runtime_args(
    endpoint: &str,
    runtime_id: &str,
    workspaces: &[String],
    plugins_dir: Option<&Path>,
    hook_path: &[PathBuf],
) -> Vec<String> {
    let mut args = vec![
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--runtime-id".to_string(),
        runtime_id.to_string(),
    ];
    for w in workspaces {
        args.push("--workspace".to_string());
        args.push(w.clone());
    }
    if let Some(dir) = plugins_dir {
        args.push("--plugins-dir".to_string());
        args.push(dir.display().to_string());
        for hp in hook_path {
            args.push("--hook-path".to_string());
            args.push(hp.display().to_string());
        }
    }
    args
}

/// One-line note about the host plugin library, printed when connecting with one.
pub fn plugins_summary(plugins_dir: &Path, count: usize) -> String {
    format!("plugins: {count} installed from {}", plugins_dir.display())
}
```

Change `run`'s signature (add two trailing params) and body to use them:

```rust
pub fn run(
    runtime_bin: &Path,
    server: &str,
    workspaces: &[String],
    runtime_id: &str,
    background: bool,
    state_dir: &Path,
    plugins_dir: Option<PathBuf>,
    hook_path: Vec<PathBuf>,
) -> Result<i32, CliError> {
    let endpoint = server_to_endpoint(server)?;
    let normalized: Vec<String> = workspaces
        .iter()
        .map(|w| normalize_workspace_arg(w))
        .collect();
    let args = runtime_args(
        &endpoint,
        runtime_id,
        &normalized,
        plugins_dir.as_deref(),
        &hook_path,
    );

    let mut cmd = Command::new(runtime_bin);
    cmd.args(&args);

    println!("{}", connection_summary(server, runtime_id, &normalized));
    if let Some(dir) = &plugins_dir {
        println!("{}", plugins_summary(dir, crate::plugins::count_installed(dir)));
    }
    println!("open {server} in your browser to start a session");
    // ... background/foreground spawn logic unchanged ...
}
```

(The `if background { ... } else { ... }` block from `state_dir` onward is untouched.)

- [ ] **Step 4: Wire the dispatch in `cli/src/main.rs`**

Replace the `Command::Connect` arm (lines 550-570):

```rust
        Command::Connect {
            server,
            workspace,
            runtime_id,
            background,
            config,
        } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg
                .runtime
                .bin
                .clone()
                .unwrap_or_else(daemon::default_runtime_bin);
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &runtime_id,
                background,
                &cfg.storage.state_dir,
            )
        }
```

with:

```rust
        Command::Connect {
            server,
            workspace,
            runtime_id,
            background,
            config,
        } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg
                .runtime
                .bin
                .clone()
                .unwrap_or_else(daemon::default_runtime_bin);
            let (plugins_dir, hook_path) = horsie::plugins::library_for_runtime(
                &cfg.storage.plugins_dir,
                cfg.runtime.hook_path.clone(),
            );
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &runtime_id,
                background,
                &cfg.storage.state_dir,
                plugins_dir,
                hook_path,
            )
        }
```

(`horsie::plugins` is how main.rs already refers to the lib crate's plugins module — see the `PluginAction::Remove` arm using `horsie::plugins::remove`.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie --lib`
Expected: PASS, all new and pre-existing tests

- [ ] **Step 6: Commit**

```bash
git add cli/src/connect.rs cli/src/main.rs
git commit -m "connect: pass the CLI plugin library to the dialed-in runtime"
```

---

### Task 3: Docs — skills guide + vendor guide reflect the local runtime loading the host library

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`
- Modify: `docs/guide/runtime-vendors.md`

**Interfaces:** none (docs only). Framing rules (from the guides' conventions): product = "horsie server", local = own-machine vendor, velos = managed vendor.

- [ ] **Step 1: Update `docs/guide/skills-and-plugins.md`**

Replace the second paragraph (lines 7-10):

```markdown
Bundles are provisioned into the sandbox at session start, so they need a
**provisioning runtime** — the **velos** vendor. The local runtime doesn't
install bundles. See [Runtime vendors](runtime-vendors.md).
```

with:

```markdown
Bundles are provisioned into the sandbox at session start, so they need a
**provisioning runtime** — the **velos** vendor. The local runtime doesn't
install server bundles, but it can load skills from a plugin library on its own
machine — see [Skills on your own machine](#skills-on-your-own-machine-host-library).
See [Runtime vendors](runtime-vendors.md).
```

Add a new section between "Use bundles in a session" and "Notes":

```markdown
## Skills on your own machine (host library)

If you run the **local** vendor (`horsie connect`), the runtime loads skills
from a plugin library on that machine instead of server bundles:

1. Install plugins with the CLI: `horsie plugin install <git-url>`
   (`horsie plugin list` / `update` / `remove` manage the library).
2. Start `horsie connect` as usual — it passes the library to the runtime
   automatically. The confirmation line shows `plugins: N installed from …`.

Every session on that runtime then sees the library's skills, and plugin
`SessionStart` hooks run on your machine when a session starts. Installs and
updates are picked up on the next session scan — no reconnect needed.

This is all-or-none: the whole library applies to every session on the runtime,
independently of the server's bundle library (the Skills page remains
velos-only).

> Hooks execute with the runtime's privileges on your machine — only install
> plugins you trust.
```

Replace the last bullet under "Notes":

```markdown
- The **local** runtime does not provision bundles, so the Skills options are
  hidden for sessions using it. Use velos to run sessions with skill bundles.
```

with:

```markdown
- The **local** runtime does not provision server bundles, so the Skills
  options are hidden for sessions using it — but it loads the CLI-installed
  host library (above). Use velos for per-session bundle selection.
```

- [ ] **Step 2: Update `docs/guide/runtime-vendors.md`**

In the comparison table (line 9), change the **local** row's "Repos & skill bundles" cell from `✗ not supported` to:

```markdown
| **local** | **Your own machine** — a daemon you run, dialing back to the server | You | ✗ repos/bundles; ✓ skills from a CLI-installed library | Working against code already on your machine |
```

In "What the local vendor does *not* do" (~line 55), replace:

```markdown
**What the local vendor does *not* do:** it can't check out GitHub repos or
install skill/plugin bundles, and it works in the fixed directory you gave it
(there's no per-session provisioning).
```

with:

```markdown
**What the local vendor does *not* do:** it can't check out GitHub repos or
install server-managed skill bundles per session, and it works in the fixed
directory you gave it (there's no per-session provisioning). It *can* load
skills from a plugin library you install on the machine with
`horsie plugin install` — see
[Skills & plugins](skills-and-plugins.md#skills-on-your-own-machine-host-library).
```

- [ ] **Step 3: Commit**

```bash
git add docs/guide/skills-and-plugins.md docs/guide/runtime-vendors.md
git commit -m "docs: local runtime loads the CLI-installed plugin library"
```

---

### Task 4: Full gate + manual verification

**Files:** none (verification only).

- [ ] **Step 1: Format, lint, test**

Run from the worktree root:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. Fix any fallout (e.g. fmt drift from the edits) and amend the relevant task's commit.

- [ ] **Step 2: Manual smoke (local, no server needed for arg check)**

```bash
cargo run -p horsie -- plugin install https://github.com/obra/superpowers --name sp-test
cargo run -p horsie -- connect --server http://localhost:3789 --workspace /tmp/scratch
```

Expected: the connect confirmation includes `plugins: 1 installed from <data_dir>/plugins`, and `ps` shows the spawned `horsie-runtime` carrying `--plugins-dir <data_dir>/plugins`. (A reachable server is not needed to verify the spawn line; Ctrl-C after confirming.) Clean up: `cargo run -p horsie -- plugin remove sp-test`.

- [ ] **Step 3: Manual end-to-end against a real server (homelab or local `horsie-server`)**

With the plugin still installed and `horsie connect` running against a server: create a session on the local vendor, send a message, and confirm the agent's available skills include one from the installed plugin (ask "what skills do you have?"). Then `horsie plugin install` a second plugin **without reconnecting**, start another session, and confirm it appears — this validates the live-scan path.
