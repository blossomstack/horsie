# Frictionless Self-Host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut self-host from ~11 manual steps to two one-liners — `docker compose -f docker/docker-compose.yml up -d` for the server, `horsie connect --server <url> --workspace <path>` for anyone connecting a machine to it.

**Architecture:** A new `docker/docker-compose.yml` boots the existing bundled server+web image with `local_runtime: true` pre-seeded. A new `horsie connect` CLI subcommand translates an HTTP(S) server URL into the `ws(s)://.../api/runtime/connect?register=<id>` endpoint the existing standalone `horsie-runtime` binary already dials, and spawns it — so installing one binary (`horsie`) is enough. A new `scripts/install.sh` (OS/arch-detecting) becomes installable via `curl -fsSL https://get.horsie.dev | sh` once a real GitHub release exists. `docs/guide/getting-started.md` and the new `docs/guide/self-hosting.md` are rewritten to lead with these two commands.

**Tech Stack:** Rust (clap-based CLI, `cli` package/`horsie` binary), Docker Compose, POSIX shell, Markdown docs.

## Global Constraints

- Provider/model configuration is NOT automated — stays a manual Settings-UI step (spec: Non-goals).
- `horsie connect` does not persist/remember servers across invocations — every call takes explicit `--server`/`--workspace` flags (spec: Non-goals).
- Cutting a real `v0.1.5` release tag (which triggers `publish.yml`'s crates.io publish + GitHub release) is a public, hard-to-reverse action and is explicitly OUT of scope for this plan's autonomous execution — flag it in the PR description as a manual follow-up requiring the repo owner's own tag push, do not push the tag yourself.
- Pre-PR gate (from `CLAUDE.md`): `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace` must all pass before opening the PR.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` (workspace lints); test code opts out via existing `#![cfg_attr(test, allow(...))]` patterns already present in touched files.

---

### Task 1: `docker/docker-compose.yml` + CI validation

**Files:**
- Create: `docker/docker-compose.yml`
- Modify: `.github/workflows/docker.yml` (add a `compose-validate` job)

**Interfaces:**
- Produces: a `docker/docker-compose.yml` runnable as `docker compose -f docker/docker-compose.yml up -d`, referenced by Task 4's `self-hosting.md`.

- [ ] **Step 1: Write `docker/docker-compose.yml`**

```yaml
# One-command self-host: `docker compose -f docker/docker-compose.yml up -d`.
# Pulls the published server image (server+web on one port, see
# ../docker/server.Dockerfile) and seeds `local_runtime: true` so a
# `horsie connect` from any machine has a listener to dial. Providers/models
# are NOT seeded here — add them via Settings after first boot.
name: horsie

services:
  horsie:
    image: ghcr.io/blossomstack/horsie:latest
    container_name: horsie
    restart: unless-stopped
    ports:
      - "3789:3789" # HTTP API + web UI
    configs:
      - source: horsie_config
        target: /etc/horsie/config.json
    volumes:
      - horsie-data:/data
    command:
      - --config
      - /etc/horsie/config.json
      - --addr
      - 0.0.0.0:3789
      - --web
      - /usr/local/share/horsie/web

configs:
  horsie_config:
    content: |
      {
        "local_runtime": true
      }

volumes:
  horsie-data:
```

- [ ] **Step 2: Validate the compose file parses**

Run: `docker compose -f docker/docker-compose.yml config`
Expected: prints the resolved config with no errors (no daemon/network access needed — `config` only parses and resolves the file).

- [ ] **Step 3: Add a CI validation job**

Open `.github/workflows/docker.yml`. Add a new job after `validate` (which builds the two Dockerfiles on every PR) that lints the compose file the same way, so a broken compose file is caught pre-merge:

```yaml
  compose-validate:
    name: Validate docker-compose.yml
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7
      - name: docker compose config
        run: docker compose -f docker/docker-compose.yml config
```

- [ ] **Step 4: Commit**

```bash
git add docker/docker-compose.yml .github/workflows/docker.yml
git commit -m "self-host: add docker/docker-compose.yml"
```

---

### Task 2: `horsie connect` translation logic (pure functions + unit tests)

**Files:**
- Create: `cli/src/connect.rs`
- Modify: `cli/src/lib.rs` (add `pub mod connect;`)
- Modify: `cli/src/daemon/mod.rs:33` (`pub(crate) fn default_runtime_bin()` → `pub fn default_runtime_bin()`, so `main.rs` — a separate binary crate over the same `horsie` library — can call it)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces (used by Task 3):
  - `pub fn server_to_endpoint(server: &str, runtime_id: &str) -> Result<String, CliError>`
  - `pub fn normalize_workspace_arg(s: &str) -> String`
  - `pub fn connection_summary(server: &str, runtime_id: &str, workspaces: &[String]) -> String`
  - `crate::daemon::default_runtime_bin() -> PathBuf` (now `pub`, unchanged signature)

- [ ] **Step 1: Write the failing tests, wired into the crate**

Create `cli/src/connect.rs` with just the test module first (no `use` yet —
the module doc comment Step 3 adds must be the first thing in the file, so
`use crate::error::CliError;` moves there too):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_to_endpoint_maps_http_to_ws() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789", "local").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_maps_https_to_wss() {
        assert_eq!(
            server_to_endpoint("https://horsie.example.com", "shawn-laptop").unwrap(),
            "wss://horsie.example.com/api/runtime/connect?register=shawn-laptop"
        );
    }

    #[test]
    fn server_to_endpoint_strips_trailing_slash() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789/", "local").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_rejects_non_http_scheme() {
        assert!(server_to_endpoint("ws://localhost:3789", "local").is_err());
        assert!(server_to_endpoint("localhost:3789", "local").is_err());
    }

    #[test]
    fn normalize_workspace_arg_defaults_bare_path_to_main() {
        assert_eq!(normalize_workspace_arg("."), "main=.");
        assert_eq!(normalize_workspace_arg("/home/shawn/proj"), "main=/home/shawn/proj");
    }

    #[test]
    fn normalize_workspace_arg_passes_through_name_eq_path() {
        assert_eq!(normalize_workspace_arg("api=./api"), "api=./api");
    }

    #[test]
    fn connection_summary_lists_every_workspace() {
        let summary = connection_summary(
            "http://localhost:3789",
            "local",
            &["main=.".to_string(), "api=./api".to_string()],
        );
        assert_eq!(
            summary,
            "connected to http://localhost:3789 as runtime \"local\" · \
             workspace \"main\" -> ., workspace \"api\" -> ./api"
        );
    }
}
```

Wire it into the crate now — an unregistered file isn't compiled at all, which
would make the next step report "no tests found" instead of a real compile
failure. In `cli/src/lib.rs`, add alongside the existing `pub mod` lines:

```rust
pub mod connect;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie connect:: --lib`
Expected: FAIL to compile — `cannot find function 'server_to_endpoint' in this scope` (and similarly for the other two functions).

- [ ] **Step 3: Implement the functions above the test module**

Add above `#[cfg(test)] mod tests` in `cli/src/connect.rs`:

```rust
//! `horsie connect`: wraps the standalone `horsie-runtime --endpoint ...`
//! dial-back flow (see `docs/guide/getting-started.md`) so installing one
//! binary, `horsie`, is enough to connect a machine to a session server.

use crate::error::CliError;

/// Translate a `--server` URL (`http(s)://host[:port]`) into the
/// `ws(s)://.../api/runtime/connect?register=<runtime_id>` endpoint
/// `horsie-runtime` expects.
pub fn server_to_endpoint(server: &str, runtime_id: &str) -> Result<String, CliError> {
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| CliError::Validation(format!("--server must be a URL, got '{server}'")))?;
    let ws_scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(CliError::Validation(format!(
                "--server must be http:// or https://, got '{other}://'"
            )));
        }
    };
    let rest = rest.trim_end_matches('/');
    Ok(format!(
        "{ws_scheme}://{rest}/api/runtime/connect?register={runtime_id}"
    ))
}

/// A bare path (no `=`) becomes `main=<path>`; `name=path` passes through
/// unchanged. `horsie-runtime`'s own parser (`WorkspaceRegistry::parse_arg`)
/// requires `name=path`, so this is the only workspace-syntax leniency
/// `horsie connect` adds on top.
pub fn normalize_workspace_arg(s: &str) -> String {
    if s.contains('=') {
        s.to_string()
    } else {
        format!("main={s}")
    }
}

/// The one-line confirmation printed once `horsie-runtime` is launched.
/// `workspaces` are already-normalized `name=path` strings.
pub fn connection_summary(server: &str, runtime_id: &str, workspaces: &[String]) -> String {
    let list = workspaces
        .iter()
        .map(|w| {
            let (name, path) = w.split_once('=').unwrap_or(("main", w.as_str()));
            format!("workspace \"{name}\" -> {path}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("connected to {server} as runtime \"{runtime_id}\" · {list}")
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie connect:: --lib`
Expected: PASS (7 tests: `server_to_endpoint_maps_http_to_ws`, `..._maps_https_to_wss`, `..._strips_trailing_slash`, `..._rejects_non_http_scheme`, `normalize_workspace_arg_defaults_bare_path_to_main`, `..._passes_through_name_eq_path`, `connection_summary_lists_every_workspace`).

- [ ] **Step 5: Promote `default_runtime_bin` for `connect` to use**

In `cli/src/daemon/mod.rs:33`, change:

```rust
pub(crate) fn default_runtime_bin() -> PathBuf {
```

to:

```rust
/// Locate the sibling `horsie-runtime` binary next to this executable — the
/// default when the config sets no explicit `runtime.bin`. Shared with
/// `horsie connect` (see `crate::connect`), which needs the same lookup.
pub fn default_runtime_bin() -> PathBuf {
```

- [ ] **Step 6: Run the full lib test suite and lint**

Run: `cargo test -p horsie --lib && cargo clippy -p horsie --all-targets --all-features -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add cli/src/connect.rs cli/src/lib.rs cli/src/daemon/mod.rs
git commit -m "cli: add horsie-connect URL/workspace translation helpers"
```

---

### Task 3: `horsie connect` CLI subcommand + spawn + e2e test

**Files:**
- Modify: `cli/src/connect.rs` (add the spawn function `run`)
- Modify: `cli/src/main.rs` (new `Command::Connect` variant + dispatch arm)
- Modify: `cli/Cargo.toml` (add `horsie-executor` as a `[dev-dependencies]`)
- Create: `cli/tests/connect_e2e.rs`

**Interfaces:**
- Consumes: `server_to_endpoint`, `normalize_workspace_arg`, `connection_summary` (Task 2); `crate::daemon::default_runtime_bin()` (Task 2, now `pub`); `crate::error::CliError`; `crate::config::HorsieConfig` (existing).
- Produces: `pub fn run(runtime_bin: &Path, server: &str, workspaces: &[String], runtime_id: &str, background: bool, state_dir: &Path) -> Result<i32, CliError>` in `cli/src/connect.rs`, called from `main.rs`'s `dispatch`.

- [ ] **Step 1: Add the spawn function to `cli/src/connect.rs`**

Add imports at the top of the file (above the doc comment's module-level `use` if any — place after the existing `use crate::error::CliError;`):

```rust
use std::path::Path;
use std::process::{Command, Stdio};
```

Add this function after `connection_summary` (before the `#[cfg(test)]` module):

```rust
/// Spawn `horsie-runtime` to dial `server` as this machine's runtime.
/// Foreground by default — the child inherits this process's stdio, so its
/// errors surface directly and the parent blocks until it exits or is
/// interrupted. `background` detaches it instead, with output redirected to
/// `<state_dir>/connect.log`.
pub fn run(
    runtime_bin: &Path,
    server: &str,
    workspaces: &[String],
    runtime_id: &str,
    background: bool,
    state_dir: &Path,
) -> Result<i32, CliError> {
    let endpoint = server_to_endpoint(server, runtime_id)?;
    let normalized: Vec<String> = workspaces.iter().map(|w| normalize_workspace_arg(w)).collect();

    let mut cmd = Command::new(runtime_bin);
    cmd.arg("--endpoint")
        .arg(&endpoint)
        .arg("--runtime-id")
        .arg(runtime_id);
    for w in &normalized {
        cmd.arg("--workspace").arg(w);
    }

    println!("{}", connection_summary(server, runtime_id, &normalized));
    println!("open {server} in your browser to start a session");

    if background {
        std::fs::create_dir_all(state_dir).map_err(|e| CliError::Io(e.to_string()))?;
        let log_path = state_dir.join("connect.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| CliError::Io(e.to_string()))?;
        let err_log = log.try_clone().map_err(|e| CliError::Io(e.to_string()))?;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log));
        let child = cmd.spawn().map_err(|e| spawn_error(runtime_bin, &e))?;
        println!(
            "running in background (pid {}, log at {})",
            child.id(),
            log_path.display()
        );
        Ok(0)
    } else {
        let status = cmd.status().map_err(|e| spawn_error(runtime_bin, &e))?;
        Ok(status.code().unwrap_or(1))
    }
}

fn spawn_error(runtime_bin: &Path, e: &std::io::Error) -> CliError {
    CliError::Executor(format!(
        "failed to launch horsie-runtime at {} ({e}); reinstall the CLI so \
         horsie-runtime is installed alongside horsie",
        runtime_bin.display()
    ))
}
```

- [ ] **Step 2: Add the `Connect` subcommand to `cli/src/main.rs`**

In the `Command` enum (`cli/src/main.rs:34`), add a new variant after `Plugin`:

```rust
    /// Dial a session server as this machine's runtime — wraps the standalone
    /// `horsie-runtime --endpoint ...` flow so installing `horsie` is enough.
    Connect {
        /// `http(s)://host:port` of the session server to dial.
        #[arg(long)]
        server: String,
        /// Repeatable `[name=]path` workspace root. A bare path defaults to
        /// name "main". At least one is required.
        #[arg(long = "workspace", required = true)]
        workspace: Vec<String>,
        /// Runtime label the server groups sessions under. Defaults to
        /// "local", matching the server's default vendor pickup.
        #[arg(long, default_value = "local")]
        runtime_id: String,
        /// Run detached, with output redirected to `<state>/connect.log`.
        #[arg(long)]
        background: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
```

Add `use horsie::connect;` to the `use` block at the top of `cli/src/main.rs` (alongside the existing `use horsie::daemon;`).

In `dispatch` (`cli/src/main.rs:366`), add a new match arm after the `Command::Plugin { action } => match action { ... }` block:

```rust
        Command::Connect {
            server,
            workspace,
            runtime_id,
            background,
            config,
        } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg.runtime.bin.clone().unwrap_or_else(daemon::default_runtime_bin);
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

- [ ] **Step 3: Confirm it builds and `--help` reflects the new subcommand**

Run: `cargo build -p horsie --bin horsie && ./target/debug/horsie connect --help`
Expected: builds cleanly; help text lists `--server`, `--workspace`, `--runtime-id`, `--background`, `--config`.

- [ ] **Step 4: Add an e2e test dependency to `cli/Cargo.toml`**

In the `[dev-dependencies]` section of `cli/Cargo.toml`, add:

```toml
horsie-executor = { path = "../executor" }
```

This is a normal (library) dependency edge, so it builds like any other — no cross-package binary-artifact trickery needed. `horsie-runtime`'s *binary* is a different problem, handled in Step 5: `cli` has no dependency on the `horsie-runtime` package at all (it never did — the CLI finds it as a sibling *file* at runtime, not a linked crate), so there is no `CARGO_BIN_EXE_horsie-runtime` available to `cli`'s tests. (That env var is only ever set for binaries of the package being tested — real for `runtime/tests/provision_steps.rs`, which lives inside the `horsie-runtime` package itself; not real for anything in `cli/tests/`.) `cli/tests/sandbox_e2e.rs` already solves exactly this problem for its own sandboxed-runtime e2e test — Step 5 reuses its approach.

- [ ] **Step 5: Write the e2e test**

Create `cli/tests/connect_e2e.rs`:

```rust
//! `horsie connect` spawns a real `horsie-runtime` binary that dials a real
//! (fake) session-server listener and announces itself under the given
//! runtime id — the same wire behavior a real session server's
//! `/api/runtime/connect` endpoint expects.
//!
//! `horsie-runtime`'s binary isn't a build dependency of `cli` (see
//! `cli/src/daemon/mod.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
//! `CARGO_BIN_EXE_horsie-runtime` for this test to use. `locate_runtime_bin`
//! mirrors the same relative-path search `cli/tests/sandbox_e2e.rs` already
//! uses for this exact problem: check next to this test binary's own
//! `target/<profile>/` dir. Only built when the workspace (or at least the
//! `runtime` package) has been compiled — skip, don't fail, if it's absent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use horsie_executor::{
    ConnectedRuntimeRegistry, RuntimeEndpoint, RuntimeListenerServer, serve_runtime_connections,
};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn locate_runtime_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?; // .../target/<profile>/deps
    if let Some(profile) = dir.parent() {
        let cand = profile.join("horsie-runtime");
        if cand.exists() {
            return Some(cand);
        }
    }
    let cand = dir.join("horsie-runtime");
    cand.exists().then_some(cand)
}

#[tokio::test]
async fn connect_dials_and_registers_under_runtime_id() {
    let Some(runtime_bin) = locate_runtime_bin() else {
        eprintln!(
            "skipping connect_dials_and_registers_under_runtime_id: horsie-runtime \
             binary not found (run via `cargo test --workspace` to build it first)"
        );
        return;
    };

    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
        .await
        .expect("bind fake server");
    let addr = listener.tcp_addr().expect("tcp addr");
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());
    let _cancel_guard = cancel.drop_guard();

    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"runtime": {{"bin": {:?}}}, "storage": {{"state_dir": {:?}}}}}"#,
            runtime_bin,
            config_dir.path().join("state"),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_horsie"))
        .args([
            "connect",
            "--server",
            &format!("http://{addr}"),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--runtime-id",
            "test-runtime",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn horsie connect");

    let mut registered = false;
    for _ in 0..100 {
        if connected.runtime_transport("test-runtime").await.is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(registered, "test-runtime never registered within 2s");

    let _ = child.kill();
    let _ = child.wait();
}
```

`CARGO_BIN_EXE_horsie` (used above for the child process itself) IS valid here — `horsie` is a bin target of the `cli` package, which is the package this test belongs to, exactly the case that env var covers.

- [ ] **Step 6: Build the workspace so `horsie-runtime` exists, then run the test**

Run: `cargo build --workspace && cargo test -p horsie --test connect_e2e`
Expected: PASS (not skipped) — the explicit `cargo build --workspace` first guarantees `target/debug/horsie-runtime` exists so `locate_runtime_bin` finds it.

- [ ] **Step 7: Run it once more scoped, to confirm the graceful-skip path doesn't panic**

Run: `rm -f target/debug/horsie-runtime && cargo test -p horsie --test connect_e2e`
Expected: PASS, with "skipping connect_dials_and_registers_under_runtime_id: horsie-runtime binary not found" on stderr — confirms the test degrades gracefully rather than failing when run in isolation. (Harmless: Step 8's `cargo test --workspace` rebuilds `horsie-runtime` anyway.)

- [ ] **Step 8: Run the full workspace pre-PR gate**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`
Expected: all PASS. Fix any formatting/lint issues the new code introduces before continuing.

- [ ] **Step 9: Commit**

```bash
git add cli/src/connect.rs cli/src/main.rs cli/Cargo.toml cli/tests/connect_e2e.rs Cargo.lock
git commit -m "cli: add horsie connect subcommand"
```

---

### Task 4: Docs — `getting-started.md` rewrite + new `self-hosting.md`

**Files:**
- Modify: `docs/guide/getting-started.md` (rewrite)
- Create: `docs/guide/self-hosting.md`
- Modify: `docs/guide/README.md` (add the new guide to the index, if it lists pages)

**Interfaces:**
- Consumes: the exact `docker compose` command from Task 1, the exact `horsie connect` flags from Task 3.
- Produces: nothing consumed by later tasks (docs are a leaf).

- [ ] **Step 1: Check whether `docs/guide/README.md` lists guide pages**

Run: `cat docs/guide/README.md`

If it's an index with links to `getting-started.md`, `sessions.md`, etc., add a line linking `self-hosting.md` (placed right after `getting-started.md`'s entry, since self-hosting is the prerequisite for someone standing up their own server). If it's not an index (e.g. just a landing paragraph with no per-page links), skip this file.

- [ ] **Step 2: Rewrite `docs/guide/getting-started.md`**

Replace its full contents with:

```markdown
# Getting started

This walks you from nothing to a working chat session.

## 1. Install the CLI

    curl -fsSL https://get.horsie.dev | sh

This installs a single binary, `horsie`, for your OS/arch.

## 2. Connect to a server

Someone (maybe you) runs the horsie server somewhere — see
[Self-hosting the server](self-hosting.md) if that's you. Once you have its
address:

    horsie connect --server https://horsie.example.com --workspace .

    connected to https://horsie.example.com as runtime "local" · workspace "main" -> /Users/shawn/proj
    open https://horsie.example.com in your browser to start a session

This registers your current directory as workspace `main`, dials the server,
and keeps running while it's connected — sessions can only reach this
machine while this process is up (add `--background` to detach it). Run it
again from a different directory (with a different `--workspace`) to add
another workspace, or with a different `--runtime-id` if more than one
machine connects to the same server.

## 3. Open the web UI, create a session

Browse to the server's URL. On first visit you'll need a provider/model in
**Settings** (your admin may have already done this). Then **New** → pick a
model → **Create**, and start chatting. Press **Stop** to interrupt a run.

From here:

- [Sessions](sessions.md) — everything the chat view and New Session dialog offer.
- [GitHub](github.md) — run sessions against real repositories.
- [MCP servers](mcp-servers.md) and [Skills & plugins](skills-and-plugins.md) —
  give agents more tools and capabilities.

## Manual / advanced setup

`horsie connect` wraps a standalone `horsie-runtime --endpoint ws://... --runtime-id ... --workspace ...` process — the two are equivalent, and the manual form is still useful if you don't want the `horsie` CLI's other subcommands, or need flags `connect` doesn't expose yet (`--sandbox-caps`, `--plugins-dir`, `--hook-path`). See [Runtime vendors](runtime-vendors.md) for the full picture, including the managed **velos** vendor option.

Building the CLI from source instead of the install script:

    make build-cli
    make install-cli

## Troubleshooting

- **"Couldn't load settings / bundles"** in the UI — the browser can't reach
  the server. Confirm it's running and that the URL you opened matches its
  `--addr`.
- **A session won't run a turn** — you have no active runtime. Check that
  `horsie connect` (or a velos vendor) is connected and that a model is set.
  See [Runtime vendors](runtime-vendors.md).
- **`403` when connecting** — the shared local runtime is disabled. Set
  `"local_runtime": true` in the server's config and restart it (already the
  default in `docker/docker-compose.yml`).
```

- [ ] **Step 3: Write `docs/guide/self-hosting.md`**

Create:

```markdown
# Self-hosting the server

From a checkout of this repo:

    docker compose -f docker/docker-compose.yml up -d

Starts the server + web UI on port 3789, no external database, no manual
config file. Data persists in a `horsie-data` Docker volume.

Next: open http://localhost:3789 → **Settings** → add a provider + model.
Then have anyone who'll run sessions against a repo on their machine follow
[Getting started](getting-started.md) to install the CLI and `horsie connect`.

## Manual / advanced setup

Building the server image or binary yourself instead of using the published
one, writing your own `config.json`, or running behind your own reverse
proxy / auth layer — all still work exactly as before:

**Build the image:**

    docker build -f docker/server.Dockerfile -t horsie-server:latest .

**Or build the binary from source** (needs a recent Rust toolchain):

    make build-server      # builds ./target/release/horsie-server
    make install-server    # optional: install it into ~/.local/bin

**`config.json`** holds only deployment settings (storage locations, the
database URL, plugin hook paths — see the [Settings reference](settings-reference.md)).
Everything you tune later lives in the Settings UI. `docker/docker-compose.yml`
seeds the minimal `{"local_runtime": true}` for you; if you're running the
binary directly, write that file yourself and pass `--config <path>`.

**Security:** there is no authentication. Only bind `0.0.0.0` on a trusted
network, or front the server with your own auth proxy.
```

- [ ] **Step 4: Proofread cross-links**

Run: `grep -rn "getting-started.md\|self-hosting.md" docs/guide/` and confirm every link target exists and every relative path resolves (both files live in `docs/guide/`, so `self-hosting.md`/`getting-started.md` with no directory prefix is correct from within either file).

- [ ] **Step 5: Commit**

```bash
git add docs/guide/getting-started.md docs/guide/self-hosting.md docs/guide/README.md
git commit -m "docs: rewrite getting-started, add self-hosting guide"
```

(Drop `docs/guide/README.md` from the `git add` if Step 1 found nothing to change there.)

---

### Task 5: `scripts/install.sh`

**Files:**
- Create: `scripts/install.sh`

**Interfaces:**
- Consumes: the release-asset naming convention already defined in `.github/workflows/publish.yml`'s `release-binaries` job: `horsie-<tag>-<target>.tar.gz`, where `<target>` is one of `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, containing the three binaries `horsie`, `horsie-runtime`, `horsie-server`.
- Produces: nothing consumed by other tasks in this plan — this script has no real release to install from until the repo owner pushes a `v*` tag (see Global Constraints). It's committed ready to use.

- [ ] **Step 1: Write `scripts/install.sh`**

```sh
#!/bin/sh
# Installs the `horsie` CLI: detects OS/arch, downloads the matching release
# tarball from the latest GitHub release, and extracts just `horsie` (not
# horsie-runtime/horsie-server — the CLI subcommand `horsie connect` spawns
# its own sibling horsie-runtime, downloaded separately, see below) into
# ~/.local/bin.
#
# Usage: curl -fsSL https://get.horsie.dev | sh
set -eu

REPO="blossomstack/horsie"
BINDIR="${BINDIR:-$HOME/.local/bin}"

os() {
  case "$(uname -s)" in
    Linux) echo "unknown-linux-gnu" ;;
    Darwin) echo "apple-darwin" ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
  esac
}

arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64" ;;
    arm64|aarch64) echo "aarch64" ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
}

target="$(arch)-$(os)"
latest_tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | \
  grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
if [ -z "$latest_tag" ]; then
  echo "could not determine the latest release of ${REPO}" >&2
  exit 1
fi

url="https://github.com/${REPO}/releases/download/${latest_tag}/horsie-${latest_tag}-${target}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading ${url}"
curl -fsSL "$url" -o "$tmp/horsie.tar.gz"
tar -xzf "$tmp/horsie.tar.gz" -C "$tmp" horsie

mkdir -p "$BINDIR"
install -m 0755 "$tmp/horsie" "$BINDIR/horsie"
echo "installed horsie to ${BINDIR}/horsie"

case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo "note: ${BINDIR} is not on your PATH — add it, e.g. export PATH=\"${BINDIR}:\$PATH\"" ;;
esac
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x scripts/install.sh`

- [ ] **Step 3: Shellcheck it**

Run: `shellcheck scripts/install.sh` (skip if `shellcheck` isn't installed locally — the CI step in Step 4 is the enforced gate).
Expected: no warnings. Fix any it reports (common ones: quote `$tmp`/`$url` — already done above; double-check after any edits).

- [ ] **Step 4: Add a CI lint step**

Add a new job to `.github/workflows/docker.yml` (or a new small workflow `.github/workflows/shellcheck.yml` — prefer the latter since it's not docker-specific):

Create `.github/workflows/shellcheck.yml`:

```yaml
name: Shellcheck

on:
  pull_request:
    paths:
      - "scripts/**"
  workflow_dispatch:

jobs:
  shellcheck:
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7
      - run: shellcheck scripts/install.sh
```

- [ ] **Step 5: Commit**

```bash
git add scripts/install.sh .github/workflows/shellcheck.yml
git commit -m "scripts: add install.sh for the horsie CLI"
```

---

### Task 6: PR

**Files:** none (repo-level action)

- [ ] **Step 1: Push the branch**

Run: `git push -u origin feat/frictionless-self-host`

- [ ] **Step 2: Open the PR**

Use `gh pr create` with a body summarizing the two one-liners, and an explicit callout that cutting `v0.1.5` (to make `scripts/install.sh` actually installable) is a deliberate follow-up requiring the repo owner's own tag push — not done as part of this PR (see Global Constraints).
