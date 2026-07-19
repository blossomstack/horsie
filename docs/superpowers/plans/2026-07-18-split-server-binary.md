# Split Server Binary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the horsie session server its own `horsie-server` binary, fully independent of the `horsie` CLI binary, and update every place that builds/runs the server (Docker, Makefile, the ops deploy stack, release CI) to use it.

**Architecture:** `horsie-server` (the `server` crate) becomes lib + bin, mirroring how `horsie-runtime` already works in this workspace — the existing `[[lib]]` is untouched, and a new auto-discovered bin target at `server/src/bin/horsie-server/` owns its own slim bootstrap config and CLI parsing. Capability-spec resolution (per-OS defaults, plugin-grant injection, seatbelt rules) is dropped entirely rather than duplicated — nothing enforces it since the old server-spawned sandboxed vendor was replaced by a user-launched daemon vendor (PR #8) — replaced with a fixed minimal default + identity finalizer, exactly matching the pattern the codebase's own tests already use.

**Tech Stack:** Rust 2024 edition, Cargo workspace, clap 4 (derive), tokio, axum, Docker, GitHub Actions, docker compose.

## Global Constraints

- Workspace lints (`[workspace.lints.clippy]` in the root `Cargo.toml`) deny `unwrap_used`, `expect_used`, `panic`, and `wildcard_enum_match_arm` in production code. Every new/modified `.rs` file below either avoids these constructs or opts test code out per-file with `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm))]` at the top of the file (the pattern already used by `cli/src/main.rs` and `runtime/src/main.rs`).
- Spec: `docs/superpowers/specs/2026-07-18-split-server-binary-design.md` (already committed on this branch).
- All Rust work happens in the `horsie` worktree at `/Users/xiaoguang/works/repos/bloomstack/october/horsie-split-server-binary`, branch `split-server-binary` (based on `origin/main@093f274`).
- The `ops/stacks/horsie/docker-compose.yml` change (Task 5) happens in the **separate** `ops` repo, at `/Users/xiaoguang/works/repos/bloomstack/october/ops`, directly on its primary checkout — no worktree there (it's a one-line edit to that repo's own IaC file, not horsie source).

---

## File Structure

New files:
- `server/src/bin/horsie-server/main.rs` — CLI parsing (clap) + server bootstrap/orchestration (moved and trimmed from `cli/src/serve.rs`).
- `server/src/bin/horsie-server/config.rs` — `BootConfig` (slim deployment config: storage paths, shared-local-runtime listener, settings-DB URL) + the two plugin/hook-path helpers, ported from `cli/src/config.rs` and `cli/src/plugins.rs`.

Modified files:
- `server/Cargo.toml` — add `clap` dependency (the crate gains a bin target that needs arg parsing; everything else the bin needs is already a dependency).
- `cli/src/main.rs` — remove the `Serve` subcommand and its dispatch arm.
- `cli/src/lib.rs` — remove `pub mod serve;`.
- `cli/Cargo.toml` — drop the `horsie-server` and `axum` dependencies (both were only used by `cli/src/serve.rs`).
- `docker/server.Dockerfile` — build/copy/run `horsie-server` instead of `horsie`; drop the `serve` verb from `CMD`.
- `Makefile` — fix the pre-existing broken `-p cli -p runtime` package spec in `build-cli`; add `build-server`/`install-server`/`uninstall-server`.
- `ops/stacks/horsie/docker-compose.yml` (separate repo) — drop the leading `- serve` from `command:`.
- `.github/workflows/publish.yml` — add `horsie-server` to the release-binaries build and tarball.

Deleted files:
- `cli/src/serve.rs`.

---

### Task 1: New `horsie-server` binary

**Files:**
- Create: `server/src/bin/horsie-server/config.rs`
- Create: `server/src/bin/horsie-server/main.rs`
- Modify: `server/Cargo.toml`

**Interfaces:**
- Consumes: `horsie_server::config::{DbConfigStore, StoreDeps}`, `horsie_server::http::{AppState, CapsFinalize, app}`, `horsie_server::plugins::{ArtifactStore, PluginProvisioner, PluginService, PluginStore}`, `horsie_server::sessions::spec::ServerDeps`, `horsie_server::sessions::supervisor::SessionSupervisor`, `horsie_server::github::{GithubApi, GithubService, GithubStore}`, `horsie_server::mcp::{McpService, McpStore}` (all pre-existing, unchanged lib API), `horsie_actor::{FileJournal, Journal, spawn_root}`, `horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy}`, `horsie_models::settings::ServerInfo`.
- Produces: binary `horsie-server` (package `horsie-server`, target dir `target/{release,debug}/horsie-server`) — consumed by Task 3 (Docker), Task 4 (Makefile), Task 6 (publish.yml). `config::BootConfig`, `config::BootError`, `config::plugins_dir_if_populated(&Path) -> Option<PathBuf>`, `config::resolve_hook_path(Option<Vec<PathBuf>>) -> Vec<PathBuf>` — all private to this bin target, not consumed elsewhere.

- [ ] **Step 1: Add the `clap` dependency**

Edit `server/Cargo.toml`, in the `[dependencies]` section, add (matches the version/feature set already used by `cli/Cargo.toml` and `runtime/Cargo.toml`):

```toml
clap              = { version = "4", features = ["derive"] }
```

- [ ] **Step 2: Write `config.rs` with its test module (tests first — the types don't exist yet, so this won't compile)**

Create `server/src/bin/horsie-server/config.rs`:

```rust
//! Deployment/bootstrap config for the `horsie-server` binary: storage paths,
//! the shared local-runtime-vendor listener, and the settings-database
//! location. Providers, models, and vendor instances are NOT here — they live
//! in the settings database (`horsie_server::config`), managed from the web
//! UI. Ported from `cli/src/config.rs`, trimmed to only what this binary
//! reads (no providers/models/hackamore/velos/default_vendor — those stay
//! CLI/job-daemon-only).

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("io error: {0}")]
    Io(String),
    #[error("config error: {0}")]
    Config(String),
}

/// All fields default, so `BootConfig::default()` is a valid empty config.
#[derive(Debug, Default, Deserialize)]
pub struct BootConfig {
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Address the shared local-runtime-vendor listener binds. User-launched
    /// `horsie-runtime --endpoint ws://...` daemons dial back here. `None`
    /// disables the shared local vendor entirely.
    #[serde(default)]
    pub local_runtime_listen: Option<String>,
    /// Where the session server persists its runtime-editable settings.
    #[serde(default)]
    pub database: DatabaseConfig,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Ephemeral runtime state. Defaults to `$XDG_STATE_HOME/horsie`, else
    /// `$HOME/.local/state/horsie`.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Durable session journal. Defaults to `$XDG_DATA_HOME/horsie`, else
    /// `$HOME/.local/share/horsie`.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Shared plugin library root. Defaults to `<data_dir>/plugins`.
    #[serde(default = "default_plugins_dir")]
    pub plugins_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            data_dir: default_data_dir(),
            plugins_dir: default_plugins_dir(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RuntimeConfig {
    /// Directories prepended to PATH when running plugin hooks (e.g. the node
    /// bin dir). Absent → auto-discover `node` from the ambient environment.
    #[serde(default)]
    pub hook_path: Option<Vec<PathBuf>>,
}

/// Absent → a SQLite file under the server data dir. Set `url` to a
/// `sqlite://…` path today, or a `postgres://…` URL once that backend lands.
#[derive(Debug, Default, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: Option<String>,
}

impl BootConfig {
    pub fn load(path: &Path) -> Result<Self, BootError> {
        let text = std::fs::read_to_string(path).map_err(|e| BootError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| BootError::Config(e.to_string()))
    }

    /// - `explicit` path given (the `--config` flag) → load it; a missing or
    ///   malformed file is an error, since the user asked for it by name.
    /// - no flag → load the user config at [`user_config_path`] if it exists;
    ///   otherwise fall back to an empty [`BootConfig::default`].
    pub fn resolve(explicit: Option<&Path>) -> Result<Self, BootError> {
        Self::resolve_with(explicit, user_config_path())
    }

    /// The path config would be loaded from: the explicit `--config` path,
    /// else the default user config path.
    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => user_config_path(),
        }
    }

    fn resolve_with(explicit: Option<&Path>, user_path: Option<PathBuf>) -> Result<Self, BootError> {
        match explicit {
            Some(p) => Self::load(p),
            None => match user_path {
                Some(p) if p.exists() => Self::load(&p),
                _ => Ok(Self::default()),
            },
        }
    }
}

/// `<config-dir>/horsie/config.json`, where `<config-dir>` is
/// `$XDG_CONFIG_HOME` if set, else `$HOME/.config`.
fn user_config_path() -> Option<PathBuf> {
    user_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn user_config_path_from(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let config_dir = match xdg_config_home {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(home?).join(".config"),
    };
    Some(config_dir.join("horsie").join("config.json"))
}

fn default_state_dir() -> PathBuf {
    storage_dir_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        ".local/state",
        "state",
    )
}

fn default_data_dir() -> PathBuf {
    storage_dir_from(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        ".local/share",
        "data",
    )
}

fn default_plugins_dir() -> PathBuf {
    default_data_dir().join("plugins")
}

fn storage_dir_from(
    xdg_base: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    home_subdir: &str,
    fallback_leaf: &str,
) -> PathBuf {
    match xdg_base {
        Some(x) if !x.is_empty() => PathBuf::from(x).join("horsie"),
        _ => match home {
            Some(h) if !h.is_empty() => PathBuf::from(h).join(home_subdir).join("horsie"),
            _ => PathBuf::from("./.horsie").join(fallback_leaf),
        },
    }
}

/// The plugins root iff it exists and holds at least one plugin — otherwise
/// `None`, so the shared plugin library feature stays inert.
pub fn plugins_dir_if_populated(dir: &Path) -> Option<PathBuf> {
    (dir.is_dir() && count_installed(dir) > 0).then(|| dir.to_path_buf())
}

fn count_installed(plugins_dir: &Path) -> usize {
    std::fs::read_dir(plugins_dir)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0)
}

/// Resolve the hook interpreter dirs: the configured override, else
/// auto-discover `node` from the ambient environment (its parent dir).
pub fn resolve_hook_path(configured: Option<Vec<PathBuf>>) -> Vec<PathBuf> {
    if let Some(paths) = configured {
        return paths;
    }
    which_dir("node").into_iter().collect()
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_empty_but_valid() {
        let cfg = BootConfig::default();
        assert_ne!(cfg.storage.state_dir, cfg.storage.data_dir);
        assert!(cfg.database.url.is_none());
        assert!(cfg.local_runtime_listen.is_none());
    }

    #[test]
    fn parses_local_runtime_listen() {
        let cfg: BootConfig =
            serde_json::from_str(r#"{ "local_runtime_listen": "0.0.0.0:3790" }"#).unwrap();
        assert_eq!(cfg.local_runtime_listen.as_deref(), Some("0.0.0.0:3790"));
    }

    #[test]
    fn parses_database_url() {
        let cfg: BootConfig =
            serde_json::from_str(r#"{ "database": { "url": "sqlite://x.db" } }"#).unwrap();
        assert_eq!(cfg.database.url.as_deref(), Some("sqlite://x.db"));
    }

    #[test]
    fn resolve_loads_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        std::fs::write(&path, r#"{ "database": { "url": "sqlite://x.db" } }"#).unwrap();
        let cfg = BootConfig::resolve(Some(&path)).unwrap();
        assert_eq!(cfg.database.url.as_deref(), Some("sqlite://x.db"));
    }

    #[test]
    fn resolve_errors_on_missing_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(BootConfig::resolve(Some(&missing)).is_err());
    }

    #[test]
    fn resolve_with_defaults_when_user_config_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        let cfg = BootConfig::resolve_with(None, Some(missing)).unwrap();
        assert!(cfg.database.url.is_none());
        let cfg = BootConfig::resolve_with(None, None).unwrap();
        assert!(cfg.database.url.is_none());
    }

    #[test]
    fn user_config_path_prefers_xdg() {
        let p = user_config_path_from(Some("/xdg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/xdg/horsie/config.json")));
    }

    #[test]
    fn plugins_dir_if_populated_requires_at_least_one_plugin() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plugins_dir_if_populated(dir.path()).is_none());
        std::fs::create_dir(dir.path().join("sp")).unwrap();
        assert_eq!(
            plugins_dir_if_populated(dir.path()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn resolve_hook_path_prefers_override() {
        let p = resolve_hook_path(Some(vec![PathBuf::from("/opt/node/bin")]));
        assert_eq!(p, vec![PathBuf::from("/opt/node/bin")]);
        assert!(resolve_hook_path(Some(vec![])).is_empty());
    }
}
```

- [ ] **Step 3: Confirm it doesn't build yet (no bin target registered)**

Run: `cargo check -p horsie-server --bin horsie-server 2>&1`
Expected: FAIL — `error: no bin target named `horsie-server`` (Cargo won't discover `src/bin/horsie-server/` as a bin target until `main.rs` exists alongside it).

- [ ] **Step 4: Write `main.rs`**

Create `server/src/bin/horsie-server/main.rs`:

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

//! `horsie-server`: the standalone session server (HTTP + SSE).
//!
//! Deployment/bootstrap config comes from `config.json`/env (storage, the
//! shared local-runtime-vendor listener, and the settings-DB location). The
//! runtime-editable settings — providers, models, vendors, default vendor —
//! live in the settings database (owned by the `horsie-server` library) and
//! are managed from the web UI, never overlapping with the file.

mod config;

use clap::Parser;
use config::{BootConfig, BootError};
use horsie_actor::{FileJournal, Journal, spawn_root};
use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
use horsie_models::settings::ServerInfo;
use horsie_server::config::{DbConfigStore, StoreDeps};
use horsie_server::http::{AppState, CapsFinalize, app};
use horsie_server::plugins::{ArtifactStore, PluginService, PluginStore};
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::SessionSupervisor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "horsie-server",
    version,
    about = "Session-oriented HTTP + SSE server for horsie"
)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Bind address for the HTTP server. Use `0.0.0.0:3789` to accept
    /// connections from other hosts on the network.
    #[arg(long, default_value = "127.0.0.1:3789")]
    addr: String,
    /// Directory of built web-UI assets to serve alongside the API (e.g.
    /// `clients/web/dist`). When set, the UI is served same-origin, so no
    /// separate dev server or CORS setup is needed.
    #[arg(long)]
    web: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), BootError> {
    let cfg = BootConfig::resolve(cli.config.as_deref())?;
    let config_path = BootConfig::resolve_path(cli.config.as_deref());

    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| BootError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| BootError::Io(e.to_string()))?;

    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));

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

    let plugins_dir = config::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        config::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else {
        Vec::new()
    };

    let db_url = resolve_db_url(&cfg, &data_dir);
    let info = ServerInfo {
        config_path: config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        database: redact_db_url(&db_url),
        state_dir: cfg.storage.state_dir.display().to_string(),
        data_dir: cfg.storage.data_dir.display().to_string(),
        plugins_dir: cfg.storage.plugins_dir.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let opened = DbConfigStore::open(
        &db_url,
        StoreDeps {
            info,
            local_runtime_listen: cfg.local_runtime_listen.clone(),
        },
    )
    .await
    .map_err(BootError::Config)?;

    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.pool.clone()),
        github.clone(),
    ));
    let plugins = Arc::new(PluginService::new(
        PluginStore::new(opened.pool.clone()),
        ArtifactStore::new(data_dir.join("plugins")),
        artifact_secret(),
    ));

    let deps = ServerDeps {
        provider_registry: opened.registry,
        vendors: opened.vendors,
        state_dir,
        github_tokens: Some(github.clone()),
        mcp: Some(mcp.clone()),
        plugins: Some(plugins.clone() as Arc<dyn horsie_server::plugins::PluginProvisioner>),
    };
    let (global_tx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(
        SessionSupervisor::new(deps, global_tx.clone()),
        journal.clone(),
    );

    let state = AppState {
        supervisor,
        journal,
        global_events: global_tx,
        caps_finalize,
        default_caps,
        plugins_dir,
        hook_path,
        config_store: opened.store,
        github,
        mcp,
        plugins,
        web_dir: cli.web,
    };
    let listener = tokio::net::TcpListener::bind(&cli.addr)
        .await
        .map_err(|e| BootError::Io(format!("bind {}: {e}", cli.addr)))?;
    println!("horsie server listening on http://{}", cli.addr);
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    axum::serve(listener, app(state))
        .await
        .map_err(|e| BootError::Io(e.to_string()))
}

/// `$HORSIE_DATABASE_URL`, else `database.url` from config, else a SQLite
/// file under the server data dir.
fn resolve_db_url(cfg: &BootConfig, data_dir: &Path) -> String {
    if let Ok(v) = std::env::var("HORSIE_DATABASE_URL")
        && !v.is_empty()
    {
        return v;
    }
    if let Some(u) = cfg.database.url.as_ref().filter(|s| !s.is_empty()) {
        return u.clone();
    }
    format!("sqlite://{}/config.db", data_dir.display())
}

/// The HS256 secret for artifact capability tokens: `$HORSIE_ARTIFACT_SECRET`
/// if set, else 32 random bytes (fine per-process — tokens are short-lived).
fn artifact_secret() -> Vec<u8> {
    std::env::var("HORSIE_ARTIFACT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .map(String::into_bytes)
        .unwrap_or_else(|| {
            let mut v = uuid::Uuid::new_v4().as_bytes().to_vec();
            v.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
            v
        })
}

/// Hide credentials in a database URL's authority (e.g. `postgres://u:p@host`).
fn redact_db_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((auth, tail)) = rest.split_once('@')
        && auth.contains(':')
    {
        return format!("{scheme}://***@{tail}");
    }
    url.to_string()
}
```

- [ ] **Step 5: Run the config tests**

Run: `cargo test -p horsie-server --bin horsie-server`
Expected: PASS — all `config::tests::*` cases green.

- [ ] **Step 6: Build the binary**

Run: `cargo build -p horsie-server`
Expected: PASS. Confirm the binary exists: `ls target/debug/horsie-server`.

- [ ] **Step 7: Manually smoke-test it boots**

Run: `./target/debug/horsie-server --config /dev/null --addr 127.0.0.1:39999 &` then `sleep 1 && curl -sS http://127.0.0.1:39999/api/health; kill %1`

Expected: the curl prints a JSON health response (200), and stdout from the server showed `horsie server listening on http://127.0.0.1:39999`. (`--config /dev/null` fails to parse as JSON — if that errors, instead omit `--config` entirely so `BootConfig` falls back to its empty default.)

- [ ] **Step 8: Commit**

```bash
git add server/Cargo.toml server/src/bin/horsie-server/
git commit -m "server: add standalone horsie-server binary"
```

---

### Task 2: Remove `Serve` from the `cli` crate

**Files:**
- Modify: `cli/src/main.rs`
- Modify: `cli/src/lib.rs`
- Modify: `cli/Cargo.toml`
- Delete: `cli/src/serve.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `horsie` binary with no `serve` subcommand, no `horsie-server`/`axum` dependency.

- [ ] **Step 1: Remove the `Serve` variant from `Command`**

In `cli/src/main.rs`, delete this variant from `enum Command` (currently right after the `Plugin` variant):

```rust
    /// Run the session server (HTTP + SSE) in the foreground.
    Serve {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Bind address for the HTTP server. Use `0.0.0.0:3789` to accept
        /// connections from other hosts on the network.
        #[arg(long, default_value = "127.0.0.1:3789")]
        addr: String,
        /// Directory of built web-UI assets to serve alongside the API (e.g.
        /// `clients/web/dist`). When set, the UI is served same-origin, so no
        /// separate dev server or CORS setup is needed.
        #[arg(long)]
        web: Option<PathBuf>,
    },
```

- [ ] **Step 2: Remove its dispatch arm**

In `cli/src/main.rs`, inside `async fn dispatch`, delete:

```rust
        Command::Serve { config, addr, web } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let config_path = HorsieConfig::resolve_path(config.as_deref());
            horsie::serve::serve(cfg, config_path, addr, web).await?;
            Ok(0)
        }
```

- [ ] **Step 3: Delete `cli/src/serve.rs` and its module declaration**

```bash
rm cli/src/serve.rs
```

In `cli/src/lib.rs`, remove the line `pub mod serve;`, leaving:

```rust
pub mod capabilities;
pub mod client;
pub mod config;
pub mod daemon;
pub mod error;
pub mod plugins;
pub mod validate;
```

- [ ] **Step 4: Drop the now-unused dependencies**

In `cli/Cargo.toml`, remove these two lines from `[dependencies]`:

```toml
horsie-server = { version = "0.1.4", path = "../server" }
axum            = { workspace = true }
```

- [ ] **Step 5: Build and test**

Run: `cargo build -p horsie`
Expected: PASS.

Run: `cargo test -p horsie`
Expected: PASS (no test referenced `serve`).

Run: `cargo tree -p horsie -i horsie-server 2>&1`
Expected: `error: package ID specification `horsie-server` did not match any packages` — confirms the dependency is gone.

- [ ] **Step 6: Confirm the subcommand is gone**

Run: `./target/debug/horsie --help`
Expected: the command list shows `validate`, `daemon`, `job`, `plugin` — no `serve`.

- [ ] **Step 7: Commit**

```bash
git add cli/src/main.rs cli/src/lib.rs cli/Cargo.toml
git rm cli/src/serve.rs
git commit -m "cli: remove serve subcommand, now a standalone binary"
```

---

### Task 3: Update `docker/server.Dockerfile`

**Files:**
- Modify: `docker/server.Dockerfile`

**Interfaces:**
- Consumes: the `horsie-server` binary from Task 1 (built inside the Docker build stage, not the host).
- Produces: a Docker image whose entrypoint is `horsie-server`, consumed by Task 5 (ops compose) and Task 7 (final verification).

- [ ] **Step 1: Edit stage 2 (build)**

In `docker/server.Dockerfile`, change:

```dockerfile
# ---- Stage 2: build the horsie binary (cli crate, package `horsie`) ----------
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Cache the cargo registry/git and the target dir across builds. All three are
# cache mounts (not image layers), so the binary must be copied OUT to a normal
# path within this same RUN -- otherwise it vanishes with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p horsie \
    && cp target/release/horsie /usr/local/bin/horsie
```

to:

```dockerfile
# ---- Stage 2: build the horsie-server binary (server crate) ------------------
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Cache the cargo registry/git and the target dir across builds. All three are
# cache mounts (not image layers), so the binary must be copied OUT to a normal
# path within this same RUN -- otherwise it vanishes with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p horsie-server \
    && cp target/release/horsie-server /usr/local/bin/horsie-server
```

- [ ] **Step 2: Edit stage 3 (runtime)**

Change:

```dockerfile
COPY --from=build /usr/local/bin/horsie /usr/local/bin/horsie
```

to:

```dockerfile
COPY --from=build /usr/local/bin/horsie-server /usr/local/bin/horsie-server
```

Change:

```dockerfile
ENTRYPOINT ["horsie"]
# Sane default; the deploy stack overrides `command:` with the full invocation
# (--config /etc/horsie/config.json, etc.).
CMD ["serve", "--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]
```

to:

```dockerfile
ENTRYPOINT ["horsie-server"]
# Sane default; the deploy stack overrides `command:` with the full invocation
# (--config /etc/horsie/config.json, etc.).
CMD ["--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]
```

- [ ] **Step 3: Build the image locally**

Run: `docker build -f docker/server.Dockerfile -t horsie-server-test .`
Expected: PASS (this rebuilds the whole workspace inside the container, so it will take a few minutes the first time).

- [ ] **Step 4: Run it and confirm it serves**

Run:
```bash
docker run -d --name horsie-server-test -p 18789:3789 horsie-server-test
sleep 2
curl -sS http://127.0.0.1:18789/api/health
docker logs horsie-server-test
docker rm -f horsie-server-test
```
Expected: `curl` returns a 200 JSON health response; `docker logs` shows `horsie server listening on http://0.0.0.0:3789` and `serving web UI from /usr/local/share/horsie/web`.

- [ ] **Step 5: Commit**

```bash
git add docker/server.Dockerfile
git commit -m "docker: build horsie-server image from the standalone binary"
```

---

### Task 4: Update `Makefile`

**Files:**
- Modify: `Makefile`

**Interfaces:**
- Consumes: the `horsie-server` package built by Task 1.
- Produces: `make build-server`/`install-server`/`uninstall-server` targets; a fixed `build-cli`.

- [ ] **Step 1: Fix the broken `build-cli` package spec**

In `Makefile`, change:

```makefile
## build-cli: build the horsie CLI + its sandboxed runtime child ($(PROFILE))
build-cli:
	$(CARGO) build $(PROFILE_FLAG) -p cli -p runtime
	@echo "built: $(TARGET_DIR)/horsie  $(TARGET_DIR)/horsie-runtime"
```

to:

```makefile
## build-cli: build the horsie CLI + its sandboxed runtime child ($(PROFILE))
build-cli:
	$(CARGO) build $(PROFILE_FLAG) -p horsie -p horsie-runtime
	@echo "built: $(TARGET_DIR)/horsie  $(TARGET_DIR)/horsie-runtime"
```

(`-p cli -p runtime` used the directory names, not the actual Cargo package names — `cargo pkgid cli` fails; the real names are `horsie` and `horsie-runtime`. This target was broken before this change.)

- [ ] **Step 2: Add `build-server`/`install-server`/`uninstall-server`**

Directly below the existing `uninstall-cli` target, add:

```makefile
## build-server: build the horsie-server binary ($(PROFILE))
build-server:
	$(CARGO) build $(PROFILE_FLAG) -p horsie-server
	@echo "built: $(TARGET_DIR)/horsie-server"

## install-server: build + install horsie-server into $(BINDIR)
install-server: build-server
	@mkdir -p "$(DESTDIR)$(BINDIR)"
	install -m 0755 "$(TARGET_DIR)/horsie-server" "$(DESTDIR)$(BINDIR)/horsie-server"
	@echo "installed: $(DESTDIR)$(BINDIR)/horsie-server"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "note: $(BINDIR) is not on your PATH — add it to run \`horsie-server\` directly";; esac

## uninstall-server: remove horsie-server from $(BINDIR)
uninstall-server:
	rm -f "$(DESTDIR)$(BINDIR)/horsie-server"
	@echo "removed horsie-server from $(DESTDIR)$(BINDIR)"
```

- [ ] **Step 3: Update `.PHONY` and the top-of-file comment**

Change:

```makefile
.PHONY: build-cli build test fmt fmt-check clippy deny check ts-types web web-build install-cli uninstall-cli clean help
```

to:

```makefile
.PHONY: build-cli build-server build test fmt fmt-check clippy deny check ts-types web web-build install-cli uninstall-cli install-server uninstall-server clean help
```

Change the top-of-file comment:

```makefile
# horsie — common developer tasks.
# The CLI is two binaries: `horsie` (cli crate) spawns the sibling
# `horsie-runtime` (runtime crate), so build-cli builds both.
```

to:

```makefile
# horsie — common developer tasks.
# Three binaries: `horsie` (cli crate) spawns the sibling `horsie-runtime`
# (runtime crate) per job, so build-cli builds both. `horsie-server` (server
# crate) is the standalone session server, independent of the CLI — build it
# with build-server.
```

- [ ] **Step 4: Verify**

Run: `make build-cli`
Expected: PASS, prints `built: target/release/horsie  target/release/horsie-runtime` (this target was broken before Step 1 — confirm it now actually works).

Run: `make build-server`
Expected: PASS, prints `built: target/release/horsie-server`.

Run: `make help`
Expected: output lists `build-server`, `install-server`, `uninstall-server` alongside the existing targets.

- [ ] **Step 5: Commit**

```bash
git add Makefile
git commit -m "makefile: add horsie-server targets, fix broken build-cli package spec"
```

---

### Task 5: Update the ops deploy stack (separate repo)

**Files:**
- Modify: `/Users/xiaoguang/works/repos/bloomstack/october/ops/stacks/horsie/docker-compose.yml` (in the `ops` repo's primary checkout — not the horsie worktree, no worktree needed for this one-line ops-owned IaC edit)

**Interfaces:**
- Consumes: the `ghcr.io/blossomstack/horsie` image built by Task 3's Dockerfile (once published by CI — this task only edits the compose file and validates it locally).
- Produces: an updated `command:` invocation matching the new `ENTRYPOINT`.

- [ ] **Step 1: Drop the leading `serve` from `command:`**

In `ops/stacks/horsie/docker-compose.yml`, change:

```yaml
    command:
      - serve
      - --config
      - /etc/horsie/config.json
      - --addr
      - 0.0.0.0:3789
      - --web
      - /usr/local/share/horsie/web
```

to:

```yaml
    command:
      - --config
      - /etc/horsie/config.json
      - --addr
      - 0.0.0.0:3789
      - --web
      - /usr/local/share/horsie/web
```

- [ ] **Step 2: Validate the compose file**

Run (from `/Users/xiaoguang/works/repos/bloomstack/october/ops`): `docker compose -f stacks/horsie/docker-compose.yml config`
Expected: PASS — prints the resolved compose config with `command: ["--config", "/etc/horsie/config.json", "--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]`, no YAML/schema errors. (This will warn about the external `horsie-data` volume and `${VELOS_TOKEN}`/`${KIMI_API_KEY}` env vars not being set locally — that's expected and unrelated to this change.)

- [ ] **Step 3: Commit (in the `ops` repo)**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/ops
git add stacks/horsie/docker-compose.yml
git commit -m "horsie: drop serve verb, image now runs horsie-server directly"
```

---

### Task 6: Update `.github/workflows/publish.yml`

**Files:**
- Modify: `.github/workflows/publish.yml`

**Interfaces:**
- Consumes: the `horsie-server` package.
- Produces: release tarballs (`horsie-<tag>-<target>.tar.gz`) that include `horsie-server` alongside `horsie`/`horsie-runtime`.

- [ ] **Step 1: Add `horsie-server` to the build command**

In `.github/workflows/publish.yml`, in the `release-binaries` job's `Build` step, change:

```yaml
      - name: Build
        run: cargo build --release --locked -p horsie -p horsie-runtime --target ${{ matrix.target }}
```

to:

```yaml
      - name: Build
        run: cargo build --release --locked -p horsie -p horsie-runtime -p horsie-server --target ${{ matrix.target }}
```

- [ ] **Step 2: Add it to the release tarball**

Change:

```yaml
      - name: Package
        run: |
          tar -czf "horsie-${GITHUB_REF_NAME}-${{ matrix.target }}.tar.gz" \
            -C "target/${{ matrix.target }}/release" horsie horsie-runtime
```

to:

```yaml
      - name: Package
        run: |
          tar -czf "horsie-${GITHUB_REF_NAME}-${{ matrix.target }}.tar.gz" \
            -C "target/${{ matrix.target }}/release" horsie horsie-runtime horsie-server
```

- [ ] **Step 3: Validate YAML syntax**

Run: `python3 -c "import yaml, sys; yaml.safe_load(open('.github/workflows/publish.yml'))" && echo OK`
Expected: prints `OK` (this workflow only runs on a `v*` tag push or manual dispatch, so it can't be exercised end-to-end here — syntax validation plus a manual re-read of the diff is the available check).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/publish.yml
git commit -m "ci: include horsie-server in release binary tarballs"
```

---

### Task 7: Full verification pass

**Files:** none (verification only).

**Interfaces:** none.

- [ ] **Step 1: Whole-workspace check**

Run (from the horsie worktree root): `make check`
Expected: PASS — `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace` all succeed. This is the first point every crate (including the new `horsie-server` bin target and the trimmed `cli`) has been checked together.

- [ ] **Step 2: Confirm both binaries build independently, with no cross-dependency**

Run: `cargo build -p horsie-server -p horsie`
Expected: PASS.

Run: `cargo tree -p horsie -p horsie-server 2>&1 | grep -c "horsie-server\|-> horsie "`

Inspect the output by eye (a grep count alone isn't conclusive with two different package trees printed back to back): confirm `cargo tree -p horsie` does not list `horsie-server` as a dependency, and `cargo tree -p horsie-server` does not list `horsie` (the CLI package) as a dependency.

- [ ] **Step 3: End-to-end local deployment smoke test**

This exercises Docker + docker compose together, the way the homelab actually runs it — the concrete check for "local deployment works well after the change."

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie-split-server-binary
docker build -f docker/server.Dockerfile -t horsie-server-local .
```

Find the exact tag the compose file currently pins (`image: ghcr.io/blossomstack/horsie:sha-XXXXXXX` in `ops/stacks/horsie/docker-compose.yml`), then tag the locally-built image to match it, so the real compose file runs unmodified — no need to edit or override it for this test:

```bash
PINNED_TAG=$(grep -oP '(?<=ghcr.io/blossomstack/horsie:)\S+' \
  /Users/xiaoguang/works/repos/bloomstack/october/ops/stacks/horsie/docker-compose.yml)
docker tag horsie-server-local ghcr.io/blossomstack/horsie:"$PINNED_TAG"
```

Then, from the `ops` repo, bring the stack up against that tag (a `docker compose up` with a matching local image tag does not hit the registry) and hit it:

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/ops
docker compose -f stacks/horsie/docker-compose.yml config >/dev/null  # re-validate after Task 5's edit
docker volume create horsie_horsie-data 2>/dev/null || true
VELOS_TOKEN=unused KIMI_API_KEY=unused docker compose -f stacks/horsie/docker-compose.yml up -d
sleep 3
curl -sS http://127.0.0.1:3789/api/health
docker compose -f stacks/horsie/docker-compose.yml down
docker volume rm horsie_horsie-data
```

Expected: the `curl` returns a 200 JSON health response from the container started via the actual (edited) ops compose file, proving the Dockerfile change (Task 3) and the compose change (Task 5) work together, not just individually.

- [ ] **Step 4: No commit for this task** — it only verifies work already committed in Tasks 1–6. If any check fails, fix the relevant task's files and re-run `make check` before considering the plan complete.
