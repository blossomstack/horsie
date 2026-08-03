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
    /// Where the session server persists its runtime-editable settings.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Where actor journals live.
    #[serde(default)]
    pub journal: JournalConfig,
    /// Authentication. Enabled unless explicitly turned off — a deployment
    /// reachable from anywhere but localhost should not be open by accident.
    #[serde(default)]
    pub auth: AuthConfig,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

/// `$HORSIE_AUTH_ENABLED` if set to a recognised value, else the config file.
/// An unrecognised value falls through to the file rather than silently
/// disabling authentication.
pub fn auth_enabled(cfg: &BootConfig) -> bool {
    auth_enabled_from(cfg, std::env::var("HORSIE_AUTH_ENABLED").ok())
}

fn auth_enabled_from(cfg: &BootConfig, env: Option<String>) -> bool {
    match env.as_deref().map(str::trim) {
        Some("1" | "true" | "TRUE" | "yes") => true,
        Some("0" | "false" | "FALSE" | "no") => false,
        _ => cfg.auth.enabled,
    }
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Ephemeral runtime state. Defaults to `$XDG_STATE_HOME/horsie`, else
    /// `$HOME/.local/state/horsie`.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Durable session journal, when `journal.backend` is `file`. Defaults to
    /// `$XDG_DATA_HOME/horsie`, else `$HOME/.local/share/horsie`.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            data_dir: default_data_dir(),
        }
    }
}

/// Absent → a SQLite file under the server data dir. Set `url` to a
/// `sqlite://…` path or a `postgres://…` URL.
#[derive(Debug, Default, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: Option<String>,
    /// Pool size. Settings reads and journal writes share this pool.
    #[serde(default)]
    pub max_connections: Option<u32>,
}

/// Which journal backend to use, and how an absent setting resolves.
#[derive(Debug, Default, Deserialize)]
pub struct JournalConfig {
    #[serde(default)]
    pub backend: Option<JournalBackend>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JournalBackend {
    /// Base64 JSONL under `storage.data_dir`. Needs a durable volume.
    File,
    /// The `journal_events` / `journal_snapshots` tables in `database.url`.
    Database,
}

impl JournalBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalBackend::File => "file",
            JournalBackend::Database => "database",
        }
    }
}

/// The journal backend to use: the configured one, else `database`.
///
/// The default does not depend on the dialect. It is tempting to make it
/// asymmetric — keep `file` on SQLite so an upgrade cannot abandon journals
/// already on disk — but that ship has sailed: the database journal has been
/// the default since it landed for SQLite, so defaulting to `file` here would
/// abandon the journals of every deployment that has upgraded since, which is
/// the same one-way door pointing the other way. `file` stays available
/// explicitly, and remains what the CLI uses.
///
/// The resolved value is logged at boot and reported in `/api/config`, so it is
/// never a guess.
pub fn journal_backend(cfg: &BootConfig) -> JournalBackend {
    cfg.journal.backend.unwrap_or(JournalBackend::Database)
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

    fn resolve_with(
        explicit: Option<&Path>,
        user_path: Option<PathBuf>,
    ) -> Result<Self, BootError> {
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
        let cfg = BootConfig::default();
        assert_ne!(cfg.storage.state_dir, cfg.storage.data_dir);
        assert!(cfg.database.url.is_none());
    }

    #[test]
    fn legacy_local_runtime_key_is_tolerated() {
        // `local_runtime` was removed when user-launched runtimes became
        // always-on; old config files that still set it must keep parsing.
        let cfg: BootConfig = serde_json::from_str(r#"{ "local_runtime": true }"#).unwrap();
        assert!(cfg.database.url.is_none());
    }

    #[test]
    fn legacy_plugin_library_keys_are_tolerated() {
        // `storage.plugins_dir` and `runtime.hook_path` were removed with the
        // legacy filesystem plugin library; old files must keep parsing.
        let cfg: BootConfig = serde_json::from_str(
            r#"{ "storage": { "plugins_dir": "/x" }, "runtime": { "hook_path": ["/y"] } }"#,
        )
        .unwrap();
        assert!(cfg.database.url.is_none());
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
    fn auth_is_enabled_unless_the_config_turns_it_off() {
        let cfg = BootConfig::default();
        assert!(cfg.auth.enabled, "default is on");

        let cfg: BootConfig = serde_json::from_str(r#"{ "auth": { "enabled": false } }"#).unwrap();
        assert!(!cfg.auth.enabled);

        // An unrelated config still gets the default.
        let cfg: BootConfig =
            serde_json::from_str(r#"{ "database": { "url": "sqlite://x.db" } }"#).unwrap();
        assert!(cfg.auth.enabled);
    }

    #[test]
    fn the_env_override_beats_the_file_in_both_directions() {
        let on = BootConfig::default();
        let off: BootConfig = serde_json::from_str(r#"{ "auth": { "enabled": false } }"#).unwrap();
        // An explicit env value wins over whatever the file said.
        assert!(auth_enabled_from(&off, Some("true".into())));
        assert!(auth_enabled_from(&off, Some("1".into())));
        assert!(!auth_enabled_from(&on, Some("false".into())));
        assert!(!auth_enabled_from(&on, Some("0".into())));
        // Unset, or a value we do not recognise, falls through to the file —
        // a typo must not silently disable authentication.
        assert!(auth_enabled_from(&on, None));
        assert!(auth_enabled_from(&on, Some("maybe".into())));
        assert!(!auth_enabled_from(&off, None));
    }
}
