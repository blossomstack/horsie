use crate::error::CliError;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The session server commands target when `--server` is omitted and no
/// `default_server` is configured: the hosted service, not a local dev server.
pub const DEFAULT_SERVER: &str = "https://auth.horsie.dev";

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
    /// Session server commands use when `--server` is omitted. Managed with
    /// `horsie config set default-server`. Absent → [`DEFAULT_SERVER`].
    #[serde(default)]
    pub default_server: Option<String>,
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
    let path = HorsieConfig::resolve_path(explicit)
        .ok_or_else(|| CliError::Io("no home directory for the config file".into()))?;
    set_default_server_at(server, &path)
}

fn set_default_server_at(server: &str, path: &Path) -> Result<String, CliError> {
    let normalized = validate_server_url(server)?;
    let mut value = read_config_value(path)?;
    // A config that parses to something other than an object (e.g. an array)
    // has no keys to preserve; start over rather than index-panicking on it.
    if !value.is_object() {
        value = serde_json::json!({});
    }
    value["default_server"] = serde_json::json!(normalized);
    write_config_value(path, &value)?;
    Ok(normalized)
}

/// The configured default, `None` when absent. Does not fall back to
/// [`DEFAULT_SERVER`] — `get` reports what is stored.
pub fn get_default_server(explicit: Option<&Path>) -> Result<Option<String>, CliError> {
    let path = HorsieConfig::resolve_path(explicit)
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
    let path = HorsieConfig::resolve_path(explicit)
        .ok_or_else(|| CliError::Io("no home directory for the config file".into()))?;
    unset_default_server_at(&path)
}

fn unset_default_server_at(path: &Path) -> Result<Option<String>, CliError> {
    let mut value = read_config_value(path)?;
    // A non-object config has no `default_server` to remove; leave it untouched.
    let removed = if value.is_object() {
        value
            .get("default_server")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    } else {
        None
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove("default_server");
    }
    write_config_value(path, &value)?;
    Ok(removed)
}

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
        assert_eq!(
            cfg.storage.plugins_dir,
            cfg.storage.data_dir.join("plugins")
        );
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

    #[test]
    fn default_server_parses_when_present() {
        let cfg: HorsieConfig =
            serde_json::from_str(r#"{ "default_server": "https://auth.horsie.dev" }"#).unwrap();
        assert_eq!(
            cfg.default_server.as_deref(),
            Some("https://auth.horsie.dev")
        );
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

        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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

        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("default_server").is_none());
        assert_eq!(value["database"]["url"], "sqlite:///x.db");
    }

    #[test]
    fn set_default_server_creates_the_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.json");
        set_default_server_at("http://localhost:3789", &path).unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
        assert_eq!(
            get_default_server_at(&tmp.path().join("absent.json")).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_server_precedence_flag_over_config_over_builtin() {
        // Flag wins even when a default is configured.
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("user.json");
        std::fs::write(&cfg_path, r#"{ "default_server": "http://cfg:2" }"#).unwrap();
        assert_eq!(
            resolve_server_with(Some("http://flag:1".into()), None, Some(cfg_path.clone()))
                .unwrap(),
            "http://flag:1"
        );
        // Configured default wins over the built-in fallback.
        assert_eq!(
            resolve_server_with(None, None, Some(cfg_path)).unwrap(),
            "http://cfg:2"
        );
        // No config at all → the hosted service, not localhost.
        assert_eq!(
            resolve_server_with(None, None, None).unwrap(),
            DEFAULT_SERVER.to_string()
        );
    }
}
