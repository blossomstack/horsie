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
    }

    #[test]
    fn legacy_local_runtime_key_is_tolerated() {
        // `local_runtime` was removed when user-launched runtimes became
        // always-on; old config files that still set it must keep parsing.
        let cfg: BootConfig = serde_json::from_str(r#"{ "local_runtime": true }"#).unwrap();
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
