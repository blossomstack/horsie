# CLI Default Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users record one default session server so `horsie` commands work without `--server`, targeting the default or the hosted `https://auth.horsie.dev` instead of `127.0.0.1:3789`.

**Architecture:** A new `default_server` key in the shared `~/.config/horsie/config.json` (read-modify-write on raw JSON so the server's `BootConfig` fields survive), a new `horsie config set/get/unset default-server` command, `--default` + first-login auto-default in `horsie auth login`, and one resolution helper (`--server` flag > configured default > built-in fallback) applied in `dispatch` before any subcommand logic.

**Tech Stack:** Rust, clap 4 derive, serde_json, existing `HorsieConfig` / `Credentials` modules in the `cli` crate.

## Global Constraints

- Built-in fallback server: `https://auth.horsie.dev` (constant `DEFAULT_SERVER`).
- Resolution precedence everywhere: `--server` flag > `default_server` config > `DEFAULT_SERVER`.
- Config writes must preserve unknown JSON keys — never re-serialize `HorsieConfig` over the file.
- Stored server URLs are normalized via `auth::normalize_server` (scheme/host lowercased, trailing slash dropped).
- The default is only auto-set on the first credential; `--default` forces it; later logins without the flag never move it; logout never clears it.
- No server/wire changes. `cli/tests/connect_e2e.rs` runs unmodified.
- Pre-PR bar: `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`.

---

### Task 1: `cli/src/config.rs` — storage, validation, resolution

**Files:**
- Modify: `cli/src/config.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_SERVER: &str`
  - `HorsieConfig.default_server: Option<String>` (new field, `#[serde(default)]`)
  - `pub fn validate_server_url(s: &str) -> Result<String, CliError>` — normalized form or error
  - `pub fn set_default_server(server: &str, explicit: Option<&Path>) -> Result<String, CliError>` — returns normalized value stored
  - `pub fn get_default_server(explicit: Option<&Path>) -> Result<Option<String>, CliError>` — stored value only, no fallback
  - `pub fn unset_default_server(explicit: Option<&Path>) -> Result<Option<String>, CliError>` — value removed, if any
  - `pub fn resolve_server(flag: Option<String>, explicit: Option<&Path>) -> Result<String, CliError>`
  - test-only `fn set_default_server_at(server: &str, path: &Path) -> Result<String, CliError>`, `fn unset_default_server_at(path: &Path) -> Result<Option<String>, CliError>`, `fn get_default_server_at(path: &Path) -> Result<Option<String>, CliError>`, `fn resolve_server_with(flag: Option<String>, explicit: Option<&Path>, user_path: Option<PathBuf>) -> Result<String, CliError>`

- [ ] **Step 1: Add the field, constant, and imports**

Add `use serde_json::Value;` to the imports. Add the constant and the field to `HorsieConfig`:

```rust
/// The session server commands target when `--server` is omitted and no
/// `default_server` is configured: the hosted service, not a local dev server.
pub const DEFAULT_SERVER: &str = "https://auth.horsie.dev";
```

```rust
    /// Session server commands use when `--server` is omitted. Managed with
    /// `horsie config set default-server`. Absent → [`DEFAULT_SERVER`].
    #[serde(default)]
    pub default_server: Option<String>,
```

- [ ] **Step 2: Add validation + read-modify-write helpers**

```rust
/// Validate `s` is an `http(s)://` base URL and return its normalized form
/// (scheme/host lowercased, trailing slash dropped) for storage.
pub fn validate_server_url(s: &str) -> Result<String, CliError> {
    let scheme = s
        .split_once("://")
        .map(|(sc, _)| sc)
        .ok_or_else(|| CliError::Validation(format!("server must be a URL, got '{s}'")))?;
    match scheme {
        "http" | "https" => Ok(crate::auth::normalize_server(s)),
        other => Err(CliError::Validation(format!(
            "server must be http:// or https://, got '{other}://'"
        ))),
    }
}

/// Read the config file as raw JSON; a missing file is `{}`. Unknown keys are
/// preserved on write — the file also carries the server's `BootConfig`
/// fields, so we must never re-serialize a `HorsieConfig` over it.
fn read_config_value(path: &Path) -> Result<Value, CliError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| CliError::Config(format!("parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(CliError::Io(format!("read {}: {e}", path.display()))),
    }
}

fn write_config_value(path: &Path, value: &Value) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Io(format!("create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::Config(format!("serialize config: {e}")))?;
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| CliError::Io(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// `horsie config set default-server <url>` — record the server commands use
/// when `--server` is omitted. Returns the normalized value stored.
pub fn set_default_server(server: &str, explicit: Option<&Path>) -> Result<String, CliError> {
    let path = resolve_path(explicit)
        .ok_or_else(|| CliError::Io("no home directory for the config file".into()))?;
    set_default_server_at(server, &path)
}

fn set_default_server_at(server: &str, path: &Path) -> Result<String, CliError> {
    let normalized = validate_server_url(server)?;
    let mut value = read_config_value(path)?;
    value["default_server"] = serde_json::json!(normalized);
    write_config_value(path, &value)?;
    Ok(normalized)
}

/// The configured default, `None` when absent. Does not fall back to
/// [`DEFAULT_SERVER`] — `get` reports what is stored.
pub fn get_default_server(explicit: Option<&Path>) -> Result<Option<String>, CliError> {
    let path = resolve_path(explicit)
        .ok_or_else(|| CliError::Io("no home directory for the config file".into()))?;
    get_default_server_at(&path)
}

fn get_default_server_at(path: &Path) -> Result<Option<String>, CliError> {
    let value = read_config_value(path)?;
    Ok(value
        .get("default_server")
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

/// `horsie config unset default-server` — remove the key. Returns the value
/// removed, if any.
pub fn unset_default_server(explicit: Option<&Path>) -> Result<Option<String>, CliError> {
    let path = resolve_path(explicit)
        .ok_or_else(|| CliError::Io("no home directory for the config file".into()))?;
    unset_default_server_at(&path)
}

fn unset_default_server_at(path: &Path) -> Result<Option<String>, CliError> {
    let mut value = read_config_value(path)?;
    let removed = value
        .get("default_server")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("default_server");
    }
    write_config_value(path, &value)?;
    Ok(removed)
}
```

- [ ] **Step 3: Add resolution**

```rust
/// `--server` flag > configured `default_server` > [`DEFAULT_SERVER`].
pub fn resolve_server(flag: Option<String>, explicit: Option<&Path>) -> Result<String, CliError> {
    resolve_server_with(flag, explicit, user_config_path())
}

/// Pure core of [`resolve_server`], with the user-config path injected so the
/// precedence rules are testable without touching process env or a real home.
fn resolve_server_with(
    flag: Option<String>,
    explicit: Option<&Path>,
    user_path: Option<PathBuf>,
) -> Result<String, CliError> {
    match flag {
        Some(server) => Ok(server),
        None => {
            let cfg = HorsieConfig::resolve_with(explicit, user_path)?;
            Ok(cfg
                .default_server
                .unwrap_or_else(|| DEFAULT_SERVER.to_string()))
        }
    }
}
```

- [ ] **Step 4: Write the tests** (append to the existing `mod tests`)

```rust
    #[test]
    fn default_server_parses_when_present() {
        let cfg: HorsieConfig =
            serde_json::from_str(r#"{ "default_server": "https://auth.horsie.dev" }"#).unwrap();
        assert_eq!(cfg.default_server.as_deref(), Some("https://auth.horsie.dev"));
    }

    #[test]
    fn default_server_absent_is_none() {
        let cfg: HorsieConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.default_server.is_none());
    }

    #[test]
    fn server_urls_validate_and_normalize_for_storage() {
        assert_eq!(
            validate_server_url("https://Auth.Horsie.dev/").unwrap(),
            "https://auth.horsie.dev"
        );
        assert_eq!(
            validate_server_url("http://localhost:3789").unwrap(),
            "http://localhost:3789"
        );
        assert!(validate_server_url("ws://localhost:3789").is_err());
        assert!(validate_server_url("localhost:3789").is_err());
    }

    #[test]
    fn set_default_server_preserves_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{ "database": { "url": "sqlite:///x.db" }, "auth": { "enabled": true } }"#,
        )
        .unwrap();

        let stored = set_default_server_at("https://auth.horsie.dev/", &path).unwrap();
        assert_eq!(stored, "https://auth.horsie.dev");

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["database"]["url"], "sqlite:///x.db");
        assert_eq!(value["auth"]["enabled"], true);
        assert_eq!(value["default_server"], "https://auth.horsie.dev");
    }

    #[test]
    fn unset_default_server_removes_only_that_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{ "database": { "url": "sqlite:///x.db" }, "default_server": "https://auth.horsie.dev" }"#,
        )
        .unwrap();

        let removed = unset_default_server_at(&path).unwrap();
        assert_eq!(removed.as_deref(), Some("https://auth.horsie.dev"));

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("default_server").is_none());
        assert_eq!(value["database"]["url"], "sqlite:///x.db");
    }

    #[test]
    fn set_default_server_creates_the_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.json");
        set_default_server_at("http://localhost:3789", &path).unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["default_server"], "http://localhost:3789");
    }

    #[test]
    fn get_default_server_reads_only_the_stored_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        set_default_server_at("https://auth.horsie.dev", &path).unwrap();
        assert_eq!(
            get_default_server_at(&path).unwrap(),
            Some("https://auth.horsie.dev".to_string())
        );
        assert_eq!(get_default_server_at(&tmp.path().join("absent.json")).unwrap(), None);
    }

    #[test]
    fn resolve_server_precedence_flag_over_config_over_builtin() {
        // Flag wins even when a default is configured.
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("user.json");
        std::fs::write(&cfg_path, r#"{ "default_server": "http://cfg:2" }"#).unwrap();
        assert_eq!(
            resolve_server_with(Some("http://flag:1".into()), None, Some(cfg_path.clone())).unwrap(),
            "http://flag:1"
        );
        // Configured default wins over the built-in fallback.
        assert_eq!(
            resolve_server_with(None, None, Some(cfg_path)).unwrap(),
            "http://cfg:2"
        );
        // No config at all → the hosted service, not localhost.
        assert_eq!(resolve_server_with(None, None, None).unwrap(), DEFAULT_SERVER.to_string());
    }
```

- [ ] **Step 5: Run the config tests**

Run: `cargo test --workspace config::tests` (the repo rule: never `-p` — feature-gated tests fail)
Expected: all pass. (If single-crate filtering misbehaves, use `cargo test --workspace`.)

- [ ] **Step 6: Commit**

```bash
git add cli/src/config.rs
git commit -m "feat(cli): config default-server storage and resolution"
```

---

### Task 2: `cli/src/main.rs` — `horsie config` command + optional `--server`

**Files:**
- Modify: `cli/src/main.rs`

**Interfaces:**
- Consumes: Task 1's `config::set_default_server` / `get_default_server` / `unset_default_server` / `resolve_server`.
- Produces:
  - `Command::Config { action: ConfigAction }` subcommand
  - `enum ConfigAction { Set { key, value, config }, Get { key, config }, Unset { key, config } }`
  - `enum ConfigKey { DefaultServer }` with `fn parse(key: &str) -> Result<Self, CliError>`
  - All `--server` args across `session`/`agent`/`auth login`/`connect` become `Option<String>`, resolved once in `dispatch`.

- [ ] **Step 1: Add the `Config` command and `ConfigAction`**

In `Command`, after `Connect`, add:

```rust
    /// Read and write CLI settings stored in the user config file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
```

Add the enum (near `ConfigAction`'s siblings):

```rust
#[derive(Subcommand)]
enum ConfigAction {
    /// Set a config key. Supported keys: `default-server`.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a config key's current value.
    Get {
        key: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove a config key.
    Unset {
        key: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}
```

Add the key parser and its unit tests:

```rust
/// The config keys `horsie config` knows how to read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKey {
    DefaultServer,
}

impl ConfigKey {
    fn parse(key: &str) -> Result<Self, CliError> {
        match key {
            "default-server" => Ok(Self::DefaultServer),
            other => Err(CliError::Validation(format!(
                "unknown config key '{other}' (supported: default-server)"
            ))),
        }
    }
}
```

- [ ] **Step 2: Convert `--server` args to `Option<String>`**

Remove `default_value = "http://127.0.0.1:3789"` from all six occurrences (SessionAction::Tail/List/Status, AgentAction::List/Get/Invoke) and change `server: String` → `server: Option<String>` in each. Also:

- `AuthAction::Login`: `server: Option<String>`, plus a new field `#[arg(long)] default: bool` with doc "Make this server the default one for commands that omit --server."
- `Connect`: `server: String` → `server: Option<String>`.

- [ ] **Step 3: Resolve in dispatch**

In `dispatch`, `AuthAction::Login`:

```rust
            AuthAction::Login { server, token, default } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::auth::login(&server, token.as_deref(), default).await?;
                Ok(0)
            }
```

Session actions — e.g. `SessionAction::List { server }`:

```rust
            SessionAction::List { server } => {
                session::list(&horsie::config::resolve_server(server, None)?).await?;
                Ok(0)
            }
```

Do the same for `SessionAction::Tail`, `SessionAction::Status`, `AgentAction::List`, `AgentAction::Get`, `AgentAction::Invoke`.

`Connect` (note it already has its own `config: Option<PathBuf>`, which resolution honors):

```rust
        Command::Connect { server, workspace, name, no_sandbox, background, config } => {
            let server = horsie::config::resolve_server(server, config.as_deref())?;
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            ...
```

Add the `Command::Config` arm to the `dispatch` match:

```rust
        Command::Config { action } => match action {
            ConfigAction::Set { key, value, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    let normalized = horsie::config::set_default_server(&value, config.as_deref())?;
                    println!("default server set to {normalized}");
                    Ok(0)
                }
            },
            ConfigAction::Get { key, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    match horsie::config::get_default_server(config.as_deref())? {
                        Some(server) => {
                            println!("{server}");
                            Ok(0)
                        }
                        None => {
                            println!("no default server set");
                            Ok(0)
                        }
                    }
                }
            },
            ConfigAction::Unset { key, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    match horsie::config::unset_default_server(config.as_deref())? {
                        Some(server) => println!("removed default server {server}"),
                        None => println!("no default server set"),
                    }
                    Ok(0)
                }
            },
        },
```

- [ ] **Step 4: Add tests** (new `#[cfg(test)] mod tests` at the bottom of main.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_key_parse_accepts_default_server() {
        assert_eq!(
            ConfigKey::parse("default-server").unwrap(),
            ConfigKey::DefaultServer
        );
    }

    #[test]
    fn config_key_parse_rejects_unknown_keys() {
        let err = ConfigKey::parse("default-vendor").unwrap_err();
        assert!(format!("{err}").contains("unknown config key"), "{err}");
    }
}
```

- [ ] **Step 5: Build + run tests**

Run: `cargo test --workspace --bin horsie` (compiles main.rs and runs its tests)
Expected: compiles; main.rs tests pass.

- [ ] **Step 6: Manual smoke of the new command** (against a temp HOME)

```bash
mkdir -p /tmp/horsie-home && HOME=/tmp/horsie-home cargo run -p horsie -- config set default-server https://Auth.Horsie.dev/
HOME=/tmp/horsie-home cargo run -p horsie -- config get default-server
HOME=/tmp/horsie-home cargo run -p horsie -- config unset default-server
```

Expected: `default server set to https://auth.horsie.dev`; `https://auth.horsie.dev`; `removed default server https://auth.horsie.dev`.

- [ ] **Step 7: Commit**

```bash
git add cli/src/main.rs
git commit -m "feat(cli): horsie config set/get/unset default-server; optional --server"
```

---

### Task 3: `cli/src/auth.rs` — login `--default`, auto-default, status marker

**Files:**
- Modify: `cli/src/auth.rs`

**Interfaces:**
- Consumes: Task 1's `config::set_default_server`; Task 2 passes `default: bool` into `login`.
- Produces:
  - `pub async fn login(server: &str, token: Option<&str>, default: bool) -> Result<(), CliError>`
  - `pub fn should_default(creds_before: &Credentials, default_flag: bool) -> bool`
  - `fn marker_for(server: &str, configured_default: Option<&str>) -> String` (test-only path)

- [ ] **Step 1: Add the `HorsieConfig` import**

`use crate::config::HorsieConfig;` next to `use crate::error::CliError;`.

- [ ] **Step 2: Add `should_default` and change `login`'s signature**

```rust
/// Whether a login should become the configured default: the user asked with
/// `--default`, or this is the machine's first stored credential (nothing to
/// displace, so the first server is the natural default).
pub fn should_default(creds_before: &Credentials, default_flag: bool) -> bool {
    default_flag || creds_before.is_empty()
}
```

Change the signature and both save paths:

```rust
pub async fn login(server: &str, token: Option<&str>, default: bool) -> Result<(), CliError> {
```

Token branch — after `let mut creds = Credentials::load(&path)?;` compute `let is_default = should_default(&creds, default);`, and after `creds.save(&path)?` + the existing `println!("stored a token for …")`, add:

```rust
        if is_default {
            crate::config::set_default_server(server, None)?;
            println!("{} is now your default server", normalize_server(server));
        }
```

Device-flow completion branch — same: compute `is_default` right after loading creds (before `creds.set`), and after `creds.save(&path)?` + `println!("\nLogged in to {}.", normalize_server(server));` add the same `if is_default { … }` block.

- [ ] **Step 3: Add the `(default)` marker to `auth status`**

```rust
/// `(default)` suffix for the row matching the configured default server,
/// normalized so `https://Auth.Horsie.dev/` matches `https://auth.horsie.dev`.
/// Reads fail open: an unreadable config marks nothing.
fn default_marker(server: &str) -> String {
    let configured = match HorsieConfig::resolve(None) {
        Ok(cfg) => cfg.default_server,
        Err(_) => None,
    };
    marker_for(server, configured.as_deref())
}

fn marker_for(server: &str, configured_default: Option<&str>) -> String {
    let configured = configured_default.map(normalize_server);
    let server = normalize_server(server);
    if configured.as_deref() == Some(server.as_str()) {
        "  (default)".to_string()
    } else {
        String::new()
    }
}
```

In `status()`, change the row print to:

```rust
        println!("  {server}  —  {state}{}", default_marker(server));
```

- [ ] **Step 4: Write the tests**

```rust
    #[test]
    fn first_login_defaults_without_the_flag() {
        assert!(should_default(&Credentials::default(), false));
    }

    #[test]
    fn later_login_defaults_only_with_the_flag() {
        let mut creds = Credentials::default();
        creds.set(
            "http://x",
            ServerCredentials {
                access_token: "a".into(),
                refresh_token: String::new(),
                expires_at: 0,
            },
        );
        assert!(!should_default(&creds, false));
        assert!(should_default(&creds, true));
    }

    #[test]
    fn marker_marks_the_default_server_only() {
        assert_eq!(
            marker_for("http://localhost:3789", Some("http://localhost:3789")),
            "  (default)"
        );
        assert_eq!(
            marker_for("http://localhost:3789", Some("http://localhost:3789/")),
            "  (default)"
        );
        assert_eq!(
            marker_for("http://localhost:3789", Some("https://other.dev")),
            ""
        );
        assert_eq!(marker_for("http://localhost:3789", None), "");
    }
```

- [ ] **Step 5: Run the auth tests**

Run: `cargo test --workspace auth::tests`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add cli/src/auth.rs
git commit -m "feat(cli): auth login --default and first-login auto-default"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/guide/getting-started.md`
- Modify: `docs/guide/settings-reference.md`

- [ ] **Step 1: getting-started.md — default server subsection**

At the end of section 2 (after the `HORSIE_TOKEN` sentence), add:

```markdown
### Default server

Commands that talk to a session server — `horsie session …`, `horsie agent …`,
`horsie connect`, `horsie auth login` — take an optional `--server`. When you
omit it they target your default server: the first server you logged in to, or
whatever `horsie config set default-server <url>` records. Pass `--default` to
a login to switch the default explicitly — a later login never moves it on its
own. With no default configured, they fall back to the hosted service at
`https://auth.horsie.dev`.

    horsie config set default-server https://horsie.example.com
    horsie config get default-server
    horsie config unset default-server
```

In section 3's example, after the `open …` line, add: "If you logged in to that
server and it is your default, `horsie connect --workspace .` works too."

- [ ] **Step 2: settings-reference.md — document the CLI key**

Inside the `config.json` jsonc block, add:

```jsonc
  // CLI-only: the session server `horsie` commands use when --server is
  // omitted. Managed with `horsie config set default-server`. The server
  // ignores this key.
  "default_server": "https://horsie.example.com"
```

After the "That's the whole file." paragraph's first sentence, note: "The CLI
reads one CLI-owned key, `default_server` — managed with
`horsie config set default-server`, ignored by the server."

- [ ] **Step 3: Commit**

```bash
git add docs/guide/getting-started.md docs/guide/settings-reference.md
git commit -m "docs: default server for the CLI"
```

---

### Task 5: Full verification + PR

**Files:**
- None (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --check` (stable toolchain)
Expected: clean. If not, run `cargo fmt` and re-commit.

- [ ] **Step 2: Workspace tests**

Run: `cargo test --workspace` (the repo's testing gotcha: never `-p` for feature-gated tests — use `--workspace`)
Expected: all pass, including `cli/tests/connect_e2e.rs` (which passes `--server` explicitly, so it is unaffected).

- [ ] **Step 3: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Manual smoke**

```bash
mkdir -p /tmp/horsie-home
HOME=/tmp/horsie-home cargo run -p horsie -- auth status
HOME=/tmp/horsie-home cargo run -p horsie -- config set default-server http://localhost:3789
HOME=/tmp/horsie-home cargo run -p horsie -- session list   # should try localhost:3789 (unreachable) — confirms resolution
```

- [ ] **Step 5: Push + open PR**

```bash
git push -u origin feat/cli-default-server
```

Open the PR with a conventional title (`feat(cli): default server`) and a body
summarizing what/why plus the key callouts: raw-JSON merge-write preserving
server config, precedence rule, auto-default on first login, `--default` flag,
`(default)` marker, docs. Per repo memory, after opening, ensure CI is green and
fix any failures before considering the work done.
