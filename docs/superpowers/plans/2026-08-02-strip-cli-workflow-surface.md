# Strip Workflow Surface from the cli Crate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the local-workflow half of the `horsie` CLI (`validate`/`job`/`daemon` commands, daemon modules, workflow-related Cargo deps, dead config) so the CLI is only a session-server client (`plugin`, `session`, `connect`).

**Architecture:** Pure deletion plus two relocations: `default_runtime_bin()` moves from `cli/src/daemon/mod.rs` into `cli/src/connect.rs`, and the clap `about` string changes. `horsie-workflow`, `horsie-supervisor`, and the daemon crates stay in the workspace — only the CLI stops depending on them.

**Tech Stack:** Rust 2024, clap 4, tokio, serde. Worktree: `.horsie/worktrees/cli-strip-workflow` (branch `chore/cli-strip-workflow`, based on `origin/main` @ 0508afa).

## Global Constraints

- Workspace lints deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in production code; test code opts out per-file.
- Pre-PR checks: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check` (stable toolchain only), `cargo test --workspace`.
- Do not modify crates outside `cli/` except `docs/guide/README.md`.
- All work happens inside the worktree path `.horsie/worktrees/cli-strip-workflow`.

---

### Task 1: Delete daemon-side modules and trim main.rs

**Files:**
- Delete: `cli/src/validate.rs`, `cli/src/client.rs`, `cli/src/capabilities.rs`, `cli/src/daemon/mod.rs`, `cli/src/daemon/protocol.rs` (whole `cli/src/daemon/` dir)
- Modify: `cli/src/lib.rs`
- Rewrite: `cli/src/main.rs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `cli/src/lib.rs` exposing only `config`, `connect`, `error`, `plugins`, `session`; `cli/src/main.rs` with only `Command::{Plugin, Session, Connect}`.

- [ ] **Step 1: Delete the modules**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie/.horsie/worktrees/cli-strip-workflow
git rm -r cli/src/daemon cli/src/validate.rs cli/src/client.rs cli/src/capabilities.rs
```

- [ ] **Step 2: Rewrite lib.rs**

Replace `cli/src/lib.rs` with:

```rust
pub mod config;
pub mod connect;
pub mod error;
pub mod plugins;
pub mod session;
```

- [ ] **Step 3: Rewrite main.rs**

Replace `cli/src/main.rs` with the trimmed version below. Removed: `Command::Validate`, `Command::Daemon`, `Command::Job`, `JobAction`, `DaemonAction`, and helpers `resolve_state_dir`, `load_workflow`, `do_validate`, `build_submit`, `load_hackamore_policy`, `now_ms`, `humanize`, `active_label`, `print_job_status`, `spawn_background_daemon`, and the three hackamore unit tests. Kept: `resolve_plugin_paths`, plugin/session/connect dispatch, `#[tokio::main]` main. The `about` string now reads `"Session-server client: run this machine as a runtime vendor, tail sessions, and manage the plugin library"`. The connect arm calls `connect::default_runtime_bin()` (defined in Task 2) instead of `daemon::default_runtime_bin`.

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

use clap::{Parser, Subcommand};
use horsie::config::HorsieConfig;
use horsie::connect;
use horsie::error::CliError;
use horsie::session::{self, EventsMode};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "horsie",
    version,
    about = "Session-server client: run this machine as a runtime vendor, tail sessions, and manage the plugin library"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the shared plugin library (skills + SessionStart hooks for all jobs).
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Commands against a session server (`horsie-server`).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
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
        /// Vendor name the server publishes this machine under. Defaults to
        /// "local", matching the server's default vendor pickup.
        #[arg(long, alias = "runtime-id", default_value = "local")]
        name: String,
        /// Apply the server's sandbox policy to every runtime this agent
        /// spawns. Off by default: the machine is already your own.
        #[arg(long)]
        sandbox: bool,
        /// Removed: run the agent under a process manager instead.
        #[arg(long, hide = true)]
        background: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// Stream a session's messages to a local JSONL file until Ctrl-C.
    /// Resumes after the last recorded event when the output file exists.
    Tail {
        /// Session UUID on the server.
        session_id: String,
        /// Output file, or an existing directory to write
        /// `<session-id>.jsonl` into.
        #[arg(long)]
        output: PathBuf,
        /// Session server base URL.
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
        /// Which events to write.
        #[arg(long, value_enum, default_value = "messages")]
        events: EventsMode,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Install a plugin by cloning its git repo into the shared library.
    Install {
        /// Git URL of the plugin repo (e.g. https://github.com/obra/superpowers).
        url: String,
        /// Install name (default: derived from the URL).
        #[arg(long)]
        name: Option<String>,
        /// Git ref/branch to check out.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Reinstall over an existing plugin of the same name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List installed plugins.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Update an installed plugin (git pull).
    Update {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove an installed plugin.
    Remove {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolve the plugin library paths from config: the symlink farm
/// (`storage.plugins_dir`) and the shared clones (`<data_dir>/sources`).
fn resolve_plugin_paths(config: Option<&Path>) -> Result<horsie::plugins::PluginPaths, CliError> {
    let cfg = HorsieConfig::resolve(config)?;
    Ok(horsie::plugins::PluginPaths {
        sources: cfg.storage.data_dir.join("sources"),
        plugins: cfg.storage.plugins_dir,
    })
}

async fn dispatch(command: Command) -> Result<i32, CliError> {
    match command {
        Command::Plugin { action } => match action {
            PluginAction::Install {
                url,
                name,
                git_ref,
                force,
                config,
            } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let installed = horsie::plugins::install(&paths, &url, name, git_ref, force)?;
                println!(
                    "installed plugin '{installed}' into {}",
                    paths.plugins.display()
                );
                Ok(0)
            }
            PluginAction::List { config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let plugins = horsie::plugins::list(&paths);
                if plugins.is_empty() {
                    println!("no plugins installed");
                } else {
                    println!("{:<24} {:<10} SOURCE", "NAME", "VERSION");
                    for p in plugins {
                        println!(
                            "{:<24} {:<10} {}",
                            p.name,
                            p.version.as_deref().unwrap_or("-"),
                            p.source
                        );
                    }
                }
                Ok(0)
            }
            PluginAction::Update { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::plugins::update(&paths, &name)?;
                println!("updated plugin '{name}'");
                Ok(0)
            }
            PluginAction::Remove { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::plugins::remove(&paths, &name)?;
                println!("removed plugin '{name}'");
                Ok(0)
            }
        },
        Command::Session { action } => match action {
            SessionAction::Tail {
                session_id,
                output,
                server,
                events,
            } => {
                session::tail(&server, &session_id, &output, events).await?;
                Ok(0)
            }
        },
        Command::Connect {
            server,
            workspace,
            name,
            sandbox,
            background,
            config,
        } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg
                .runtime
                .bin
                .clone()
                .unwrap_or_else(connect::default_runtime_bin);
            let (plugins_dir, hook_path) = horsie::plugins::library_for_runtime(
                &cfg.storage.plugins_dir,
                cfg.runtime.hook_path.clone(),
            );
            let sources = cfg.storage.data_dir.join("sources");
            let plugins = plugins_dir.map(|dir| connect::PluginLibrary {
                dir,
                sources: Some(sources),
                hook_path,
            });
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &name,
                background,
                &cfg.storage.state_dir,
                plugins,
                sandbox,
            )
            .await
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli.command).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    std::process::exit(code);
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p horsie`
Expected: FAILS only on `connect::default_runtime_bin` (not yet defined, Task 2). No other errors.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(cli): drop validate/job/daemon commands and daemon modules"
```

---

### Task 2: Move `default_runtime_bin` into connect.rs

**Files:**
- Modify: `cli/src/connect.rs`

**Interfaces:**
- Consumes: nothing (uses only `std::path`).
- Produces: `pub fn default_runtime_bin() -> PathBuf` — consumed by `cli/src/main.rs` Task 1 and referenced by `cli/tests/connect_e2e.rs` doc comment (Task 4).

- [ ] **Step 1: Add the function**

Append after the `use` block in `cli/src/connect.rs` (e.g. right before `pub fn server_to_endpoint`):

```rust
/// Locate the sibling `horsie-runtime` binary next to this executable — the
/// default when the config sets no explicit `runtime.bin`. Shared with
/// `horsie connect` (see `crate::connect`), which needs the same lookup.
pub fn default_runtime_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("horsie-runtime")))
        .unwrap_or_else(|| PathBuf::from("horsie-runtime"))
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p horsie`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add cli/src/connect.rs
git commit -m "refactor(cli): move default_runtime_bin into connect"
```

---

### Task 3: Trim HorsieConfig to storage + runtime

**Files:**
- Rewrite: `cli/src/config.rs`

**Interfaces:**
- Consumes: `crate::error::CliError` (unchanged).
- Produces: `HorsieConfig` with fields `storage: StorageConfig`, `runtime: RuntimeConfig`; `pub fn load(path: &Path) -> Result<Self, CliError>`, `pub fn resolve(explicit: Option<&Path>) -> Result<Self, CliError>`, `pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf>`. Consumed by `main.rs` (Task 1) and `plugins.rs`.

- [ ] **Step 1: Rewrite config.rs**

Replace `cli/src/config.rs` with the trimmed version below. Removed: imports `horsie_agentcore`, `horsie_anthropic`, `horsie_openai`, `std::collections::HashMap`, `std::sync::Arc`; fields `providers`, `models`, `sandbox`, `hackamore`, `velos`, `default_vendor`, `local_runtime_listen`, `database`; types `DatabaseConfig`, `ProviderConfig`, `ModelConfig`, `SandboxConfig`, `HackamoreConfig`, `VelosVendorConfig`; functions `build_registry`, `build_registry_from`, `default_container_runtime_bin`, `default_workspace_root`, `default_velos_listen`, `default_velos_cpu`, `default_velos_memory_mib`, `default_velos_connect_timeout_secs`; `impl VelosVendorConfig`; and the config unit tests covering deleted fields (hackamore, velos, default_vendor, local_runtime_listen, sandbox capabilities, providers/models parsing, registry building). Kept: `HorsieConfig` (storage+runtime), `StorageConfig`, `RuntimeConfig`, `HorsieConfig::{load, resolve, resolve_path, resolve_with}`, `user_config_path`, `user_config_path_from`, `default_state_dir`, `default_data_dir`, `default_plugins_dir`, `storage_dir_from`, and the tests for storage/runtime/XDG resolution.

```rust
use crate::error::CliError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// CLI-owned policy (hand-written serde — NOT a fluorite protocol type).
///
/// All fields default, so `HorsieConfig::default()` is a valid empty config
/// (default storage/runtime). Old config files written for the daemon (with
/// providers/models/sandbox/hackamore/...) still parse: serde ignores unknown
/// JSON fields.
#[derive(Debug, Default, Deserialize)]
pub struct HorsieConfig {
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Ephemeral runtime state: the shared local-runtime-vendor socket and
    /// pidfile. Defaults to `$XDG_STATE_HOME/horsie`, else
    /// `$HOME/.local/state/horsie` (same path on macOS and Linux).
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Durable data: the shared plugin library (`plugins_dir`) and the shared
    /// clones (`<data_dir>/sources`). Defaults to `$XDG_DATA_HOME/horsie`, else
    /// `$HOME/.local/share/horsie` (same path on macOS and Linux).
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Shared plugin library root (`horsie plugin install` clones here). Exposed to
    /// opted-in agents as the `horsie_shared` workspace. Defaults to
    /// `<data_dir>/plugins`.
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
    /// Path to the `horsie-runtime` binary `horsie connect` spawns per session.
    /// Absent → the sibling `horsie-runtime` next to the running CLI executable.
    #[serde(default)]
    pub bin: Option<PathBuf>,
    /// Directories prepended to PATH when running plugin hooks (e.g. the node bin
    /// dir). Absent → auto-discover `node` from the environment. These dirs are
    /// also granted read access in the sandbox.
    #[serde(default)]
    pub hook_path: Option<Vec<PathBuf>>,
}

impl HorsieConfig {
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
    }

    /// Resolve the config per CLI policy:
    /// - `explicit` path given (the `--config` flag) → load it; a missing or
    ///   malformed file is an error, since the user asked for it by name.
    /// - no flag → load the user config at [`user_config_path`] if it exists;
    ///   otherwise fall back to an empty [`HorsieConfig::default`].
    pub fn resolve(explicit: Option<&Path>) -> Result<Self, CliError> {
        Self::resolve_with(explicit, user_config_path())
    }

    /// The path config would be loaded from / written back to: the explicit
    /// `--config` path, else the default user config path. `None` only when no
    /// home/XDG base is available (persisting settings is then impossible).
    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => user_config_path(),
        }
    }

    /// Inner policy with the user-config path injected, so the precedence rules
    /// are testable without touching process env or the real home directory.
    fn resolve_with(explicit: Option<&Path>, user_path: Option<PathBuf>) -> Result<Self, CliError> {
        match explicit {
            Some(p) => Self::load(p),
            None => match user_path {
                Some(p) if p.exists() => Self::load(&p),
                _ => Ok(Self::default()),
            },
        }
    }
}

/// The default user config path, `<config-dir>/horsie/config.json`, where
/// `<config-dir>` is `$XDG_CONFIG_HOME` if set, else `$HOME/.config`. Same path
/// on macOS and Linux. Returns `None` when neither env var is available.
fn user_config_path() -> Option<PathBuf> {
    user_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Pure core of [`user_config_path`]: prefer a non-empty `$XDG_CONFIG_HOME`,
/// else `$HOME/.config`. Returns `None` if neither yields a base directory.
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

/// Default state dir for ephemeral runtime files (control socket, pidfile):
/// `$XDG_STATE_HOME/horsie` if set, else `$HOME/.local/state/horsie`. Same path
/// on macOS and Linux.
fn default_state_dir() -> PathBuf {
    storage_dir_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        ".local/state",
        "state",
    )
}

/// Default data dir for durable data (plugin library, shared clones):
/// `$XDG_DATA_HOME/horsie` if set, else `$HOME/.local/share/horsie`. Same path
/// on macOS and Linux.
fn default_data_dir() -> PathBuf {
    storage_dir_from(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        ".local/share",
        "data",
    )
}

/// Default shared plugin library root: `<data_dir>/plugins`.
fn default_plugins_dir() -> PathBuf {
    default_data_dir().join("plugins")
}

/// Pure core of the storage-dir defaults: prefer a non-empty XDG base var joined
/// with `horsie`; else `$HOME/<home_subdir>/horsie`; else, when neither env var
/// is available (rare), a relative `./.horsie/<fallback_leaf>` so state and data
/// stay distinct without a home directory.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_empty_but_valid() {
        let cfg = HorsieConfig::default();
        // State and data resolve to distinct dirs (different XDG bases / leaves).
        assert_ne!(cfg.storage.state_dir, cfg.storage.data_dir);
        assert_eq!(cfg.storage.plugins_dir, cfg.storage.data_dir.join("plugins"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // An old daemon config (providers/models/sandbox/hackamore) still parses.
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } },
                "sandbox": { "capabilities_file": "/etc/horsie/caps.json" },
                "storage": { "state_dir": "/var/state", "data_dir": "/var/data" }
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.storage.state_dir, PathBuf::from("/var/state"));
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/var/data"));
    }

    #[test]
    fn parses_runtime_bin() {
        let cfg: HorsieConfig =
            serde_json::from_str(r#"{ "runtime": { "bin": "/opt/horsie/horsie-runtime" } }"#)
                .unwrap();
        assert_eq!(
            cfg.runtime.bin,
            Some(PathBuf::from("/opt/horsie/horsie-runtime"))
        );
    }

    #[test]
    fn runtime_bin_defaults_to_none() {
        let cfg: HorsieConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.runtime.bin.is_none());
    }

    #[test]
    fn user_config_path_prefers_xdg() {
        let p = user_config_path_from(Some("/xdg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/xdg/horsie/config.json")));
    }

    #[test]
    fn user_config_path_falls_back_to_home_dot_config() {
        // Unset and empty XDG both fall through to $HOME/.config.
        for xdg in [None, Some("".into())] {
            let p = user_config_path_from(xdg, Some("/home/u".into()));
            assert_eq!(p, Some(PathBuf::from("/home/u/.config/horsie/config.json")));
        }
    }

    #[test]
    fn user_config_path_none_without_env() {
        assert_eq!(user_config_path_from(None, None), None);
        assert_eq!(user_config_path_from(Some("".into()), None), None);
    }

    #[test]
    fn resolve_loads_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        std::fs::write(&path, r#"{ "storage": { "state_dir": "/s" } }"#).unwrap();
        let cfg = HorsieConfig::resolve(Some(&path)).unwrap();
        assert_eq!(cfg.storage.state_dir, PathBuf::from("/s"));
    }

    #[test]
    fn resolve_errors_on_missing_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(HorsieConfig::resolve(Some(&missing)).is_err());
    }

    #[test]
    fn resolve_with_loads_existing_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.json");
        std::fs::write(&path, r#"{ "runtime": { "bin": "/b" } }"#).unwrap();
        let cfg = HorsieConfig::resolve_with(None, Some(path)).unwrap();
        assert_eq!(cfg.runtime.bin, Some(PathBuf::from("/b")));
    }

    #[test]
    fn resolve_with_defaults_when_user_config_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        let cfg = HorsieConfig::resolve_with(None, Some(missing)).unwrap();
        assert!(cfg.runtime.bin.is_none());

        let cfg = HorsieConfig::resolve_with(None, None).unwrap();
        assert!(cfg.runtime.bin.is_none());
    }

    #[test]
    fn storage_dir_prefers_xdg() {
        let state = storage_dir_from(
            Some("/xdg/state".into()),
            Some("/home/u".into()),
            ".local/state",
            "state",
        );
        assert_eq!(state, PathBuf::from("/xdg/state/horsie"));
        let data = storage_dir_from(
            Some("/xdg/data".into()),
            Some("/home/u".into()),
            ".local/share",
            "data",
        );
        assert_eq!(data, PathBuf::from("/xdg/data/horsie"));
    }

    #[test]
    fn storage_dir_falls_back_to_home() {
        // Unset and empty XDG both fall through to the $HOME subdir.
        for xdg in [None, Some("".into())] {
            let p = storage_dir_from(xdg, Some("/home/u".into()), ".local/state", "state");
            assert_eq!(p, PathBuf::from("/home/u/.local/state/horsie"));
        }
        let p = storage_dir_from(None, Some("/home/u".into()), ".local/share", "data");
        assert_eq!(p, PathBuf::from("/home/u/.local/share/horsie"));
    }

    #[test]
    fn storage_dir_falls_back_to_relative_without_env() {
        // Neither XDG nor HOME → distinct relative leaves, never colliding.
        let state = storage_dir_from(None, None, ".local/state", "state");
        let data = storage_dir_from(Some("".into()), Some("".into()), ".local/share", "data");
        assert_eq!(state, PathBuf::from("./.horsie/state"));
        assert_eq!(data, PathBuf::from("./.horsie/data"));
        assert_ne!(state, data);
    }
}
```

- [ ] **Step 2: Run config unit tests**

Run: `cargo test -p horsie --lib`
Expected: PASS (config tests only; note per repo gotchas, `-p horsie` alone works because the cli crate needs no `test-util` features).

- [ ] **Step 3: Verify full crate compiles**

Run: `cargo check -p horsie`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add cli/src/config.rs
git commit -m "refactor(cli): trim HorsieConfig to what connect/plugin/session read"
```

---

### Task 4: Trim Cargo dependencies

**Files:**
- Modify: `cli/Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `cli/Cargo.toml` depending only on the crates below. Consumed by the whole crate.

- [ ] **Step 1: Remove unused dependencies**

From `[dependencies]`, remove exactly: `horsie-workflow`, `horsie-supervisor`, `horsie-actor`, `horsie-agentcore`, `horsie-anthropic`, `horsie-openai`, `horsie-runtime-client`, `eval`, `uuid`, `async-trait`, `tracing`. From `[dev-dependencies]`, remove `horsie-mock-llm`. Keep: `horsie-models`, `horsie-runtime-vendor`, `clap`, `serde`, `serde_json`, `tokio`, `tokio-util`, `thiserror`, `reqwest`, `reqwest-eventsource`, `futures-util`; dev-deps `tokio-tungstenite`, `tempfile`.

Resulting `[dependencies]` block:

```toml
[dependencies]
horsie-models = { version = "0.1.6", path = "../models" }
horsie-runtime-vendor = { version = "0.1.6", path = "../runtime-vendor" }
clap            = { version = "4", features = ["derive"] }
serde           = { workspace = true }
serde_json      = { workspace = true }
tokio           = { workspace = true, features = ["rt-multi-thread", "macros", "sync", "net", "time", "process", "signal"] }
tokio-util      = { workspace = true }
thiserror       = { workspace = true }
reqwest           = { workspace = true, features = ["stream"] }
reqwest-eventsource = { workspace = true }
futures-util      = { workspace = true }

[dev-dependencies]
tokio-tungstenite = { workspace = true }
tempfile = "3"
```

- [ ] **Step 2: Verify it compiles and tests pass**

Run: `cargo test -p horsie --lib`
Expected: PASS.

- [ ] **Step 3: Confirm no stale references**

Run: `grep -rn "horsie_actor\|horsie_agentcore\|horsie_anthropic\|horsie_openai\|horsie_supervisor\|horsie_workflow\|horsie_runtime_client\|eval::\|async_trait::\|tracing::\|uuid::" cli/src cli/tests`
Expected: no output (empty).

- [ ] **Step 4: Commit**

```bash
git add cli/Cargo.toml Cargo.lock
git commit -m "build(cli): drop workflow/daemon crate dependencies"
```

---

### Task 5: Update CLI tests

**Files:**
- Delete: `cli/tests/sandbox_e2e.rs`
- Modify: `cli/tests/connect_e2e.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `cli/tests/connect_e2e.rs` unchanged except its doc comment; no other test references the removed modules.

- [ ] **Step 1: Delete sandbox_e2e.rs**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie/.horsie/worktrees/cli-strip-workflow
git rm cli/tests/sandbox_e2e.rs
```

- [ ] **Step 2: Fix connect_e2e.rs doc comment**

In `cli/tests/connect_e2e.rs`, replace the two lines:

```
//! `horsie-runtime` isn't a build dependency of `cli` (see
//! `cli/src/daemon/mod.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
```

with:

```
//! `horsie-runtime` isn't a build dependency of `cli` (see
//! `cli/src/connect.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
```

- [ ] **Step 3: Verify cli tests compile**

Run: `cargo test -p horsie --no-run`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test(cli): drop daemon sandbox e2e, point connect e2e doc at connect.rs"
```

---

### Task 6: Update docs

**Files:**
- Modify: `docs/guide/README.md`

- [ ] **Step 1: Update the CLI note**

In `docs/guide/README.md` line 10, replace:

```
This guide covers horsie server only. It does **not** cover the separate
`horsie` CLI (`horsie job`/`horsie daemon` and workflow files) — that is a
different tool.
```

with:

```
This guide covers horsie server only. It does **not** cover the separate
`horsie` CLI — that is a different tool. Today `horsie` is a session-server
client: `horsie connect` runs this machine as a runtime vendor,
`horsie session tail` streams session events, and `horsie plugin` manages the
shared plugin library.
```

- [ ] **Step 2: Verify**

Run: `git diff -- docs/guide/README.md` — confirm only the intended hunk changed.

- [ ] **Step 3: Commit**

```bash
git add docs/guide/README.md
git commit -m "docs: describe the cli as a session-server client"
```

---

### Task 7: Full verification

**Files:** none (verification only).

- [ ] **Step 1: clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 2: fmt**

Run: `cargo fmt --check` (stable toolchain, per repo gotchas — do NOT use nightly)
Expected: PASS, no diffs.

- [ ] **Step 3: workspace tests**

Run: `cargo test --workspace`
Expected: PASS. (Note: this builds `horsie-runtime`, so `sandbox_e2e.rs`-style coverage is gone, but connect e2e remains.)

- [ ] **Step 4: CLI help smoke test**

```bash
cargo build -p horsie
./target/debug/horsie --help
```

Expected: help lists only `plugin`, `session`, `connect` subcommands.

- [ ] **Step 5: Confirm removed commands are gone**

Run: `./target/debug/horsie job --help`
Expected: exit code 2, clap error "unrecognized subcommand 'job'" (or similar), and `./target/debug/horsie daemon --help` likewise.

- [ ] **Step 6: Commit any fixes produced by this task** (e.g. fmt/clippy cleanups)

```bash
git add -A
git commit -m "chore(cli): verification fixes" || true
```
