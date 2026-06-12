use crate::error::CliError;
use agentcore::LlmProvider;
use anthropic::AnthropicProvider;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// CLI-owned policy (hand-written serde — NOT a fluorite protocol type). The
/// workflow file stays a pure `WorkflowDefinition`, reusable across server/CLI.
///
/// All fields default, so `HorsieConfig::default()` is a valid empty config
/// (no providers, no models, default storage/sandbox/runtime). An empty config
/// is a legal state — `validate` is what rejects workflows that reference models
/// the config doesn't define.
#[derive(Debug, Default, Deserialize)]
pub struct HorsieConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Optional hackamore server location. Present → a job submitted with a per-run
    /// `--hackamore-policy` mints a policy-bound proxy token at spawn (fail closed).
    /// Absent, or a job with no policy → jobs run exactly as they do today, with
    /// no hackamore env or grants.
    #[serde(default)]
    pub hackamore: Option<HackamoreConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    /// An Anthropic-API provider. The key is taken from `api_key` (inline) if set,
    /// else read from the env var named by `api_key_env`; if neither is set the
    /// client is built without auth, for a local mock server or proxy via `base_url`.
    /// Prefer `api_key_env` — it keeps the secret out of the config file.
    Anthropic {
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SandboxConfig {
    /// Capability file that fully defines the sandbox, replacing the built-in default.
    /// A `--capabilities` CLI flag overrides this. Absent → built-in default.
    #[serde(default)]
    pub capabilities_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Ephemeral runtime state: the daemon control socket, pidfile, log, and
    /// per-job capability files. Defaults to `$XDG_STATE_HOME/horsie`, else
    /// `$HOME/.local/state/horsie` (same path on macOS and Linux).
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Durable job history: the event-sourcing journal replayed to resume
    /// interrupted jobs. Defaults to `$XDG_DATA_HOME/horsie`, else
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

/// Daemon-local hackamore *server location* (hand-written serde — NOT a fluorite
/// protocol type; this never crosses a module boundary, the daemon consumes it
/// in place). hackamore is a policy-governed credential-injecting reverse proxy.
///
/// Only the deployment-global addresses live here. The policy a token is bound
/// to, and its TTL, are a per-run resource supplied by the `job run`
/// `--hackamore-policy` flag — not global config.
#[derive(Debug, Deserialize)]
pub struct HackamoreConfig {
    /// Base URL of hackamore's admin API (daemon-only; serves `POST /mint`).
    pub admin_url: String,
    /// Base URL of hackamore's proxy listener — the one address the sandboxed job
    /// may reach; injected into the runtime child as `HACKAMORE_URL`.
    pub proxy_url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RuntimeConfig {
    /// Path to the `horsie-runtime` binary the daemon spawns per job. Absent →
    /// the sibling `horsie-runtime` next to the running CLI executable.
    #[serde(default)]
    pub bin: Option<PathBuf>,
    /// Directories prepended to PATH when running plugin hooks (e.g. the node bin
    /// dir). Absent → auto-discover `node` from the daemon's environment. These dirs
    /// are also granted read access in the sandbox.
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

/// Default state dir for ephemeral runtime files (control socket, pidfile, log,
/// per-job capability files): `$XDG_STATE_HOME/horsie` if set, else
/// `$HOME/.local/state/horsie`. Same path on macOS and Linux.
fn default_state_dir() -> PathBuf {
    storage_dir_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        ".local/state",
        "state",
    )
}

/// Default data dir for the durable job journal: `$XDG_DATA_HOME/horsie` if set,
/// else `$HOME/.local/share/horsie`. Same path on macOS and Linux.
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

/// Build the provider registry keyed by **model key** (matches `WorkflowAgentDef.model`).
/// The key is resolved inline-then-env-then-none; a configured-but-missing/empty key
/// fails here, before any runtime is spawned.
pub fn build_registry(
    cfg: &HorsieConfig,
) -> Result<HashMap<String, Arc<dyn LlmProvider>>, CliError> {
    let mut reg: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    for (model_key, mc) in &cfg.models {
        let pc = cfg.providers.get(&mc.provider).ok_or_else(|| {
            CliError::Config(format!(
                "model '{model_key}' references unknown provider '{}'",
                mc.provider
            ))
        })?;
        let provider: Arc<dyn LlmProvider> = match pc {
            ProviderConfig::Anthropic {
                api_key,
                api_key_env,
                base_url,
            } => {
                // Resolve the key: inline first, then env var, else no auth.
                let resolved_key = match (api_key, api_key_env) {
                    (Some(k), _) => {
                        if k.is_empty() {
                            return Err(CliError::Config(format!(
                                "inline api_key for provider '{}' is empty",
                                mc.provider
                            )));
                        }
                        Some(k.clone())
                    }
                    (None, Some(var)) => {
                        let key = std::env::var(var).map_err(|_| {
                            CliError::Config(format!(
                                "env var '{var}' for provider '{}' is not set",
                                mc.provider
                            ))
                        })?;
                        if key.is_empty() {
                            return Err(CliError::Config(format!(
                                "env var '{var}' for provider '{}' is empty",
                                mc.provider
                            )));
                        }
                        Some(key)
                    }
                    (None, None) => None,
                };
                let mut p = match resolved_key {
                    Some(k) => AnthropicProvider::with_api_key(k)
                        .map_err(|e| CliError::Provider(e.to_string()))?,
                    None => {
                        AnthropicProvider::new().map_err(|e| CliError::Provider(e.to_string()))?
                    }
                };
                p = p.with_model(&mc.model_id).with_max_tokens(mc.max_tokens);
                if let Some(u) = base_url {
                    p = p.with_base_url(u);
                }
                Arc::new(p)
            }
        };
        reg.insert(model_key.clone(), provider);
    }
    Ok(reg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_config() {
        let json = r#"{
            "providers": { "anthropic": { "type": "anthropic", "api_key_env": "ANTHROPIC_API_KEY", "base_url": "https://api.anthropic.com" } },
            "models": { "sonnet": { "provider": "anthropic", "model_id": "claude-sonnet-4-6", "max_tokens": 8192 } },
            "sandbox": { "capabilities_file": null },
            "storage": { "state_dir": "/var/state", "data_dir": "/var/data" }
        }"#;
        let cfg: HorsieConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.providers.contains_key("anthropic"));
        assert_eq!(cfg.models["sonnet"].model_id, "claude-sonnet-4-6");
        assert_eq!(cfg.storage.state_dir, PathBuf::from("/var/state"));
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/var/data"));
    }

    #[test]
    fn inline_api_key_builds_registry_without_env() {
        // Inline key path needs no env var and no network — just constructs providers.
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "api_key": "sk-inline", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } }
            }"#,
        )
        .unwrap();
        let reg = build_registry(&cfg).expect("inline key should build");
        assert!(reg.contains_key("m"));
    }

    #[test]
    fn empty_inline_api_key_is_rejected() {
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "api_key": "" } },
                "models": { "m": { "provider": "p", "model_id": "id" } }
            }"#,
        )
        .unwrap();
        assert!(build_registry(&cfg).is_err());
    }

    #[test]
    fn parses_sandbox_capabilities_file() {
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } },
                "sandbox": { "capabilities_file": "/etc/horsie/caps.json" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            cfg.sandbox.capabilities_file,
            Some(PathBuf::from("/etc/horsie/caps.json"))
        );
    }

    #[test]
    fn capabilities_file_defaults_to_none() {
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } }
            }"#,
        )
        .unwrap();
        assert!(cfg.sandbox.capabilities_file.is_none());
    }

    #[test]
    fn parses_hackamore_section() {
        // The global section is just the deployment server location now — the
        // policy and TTL are per-run (`--hackamore-policy`).
        let cfg: HorsieConfig = serde_json::from_str(
            r#"{
                "hackamore": {
                    "admin_url": "http://127.0.0.1:9091",
                    "proxy_url": "http://127.0.0.1:9090"
                }
            }"#,
        )
        .unwrap();
        let h = cfg.hackamore.expect("hackamore section should parse");
        assert_eq!(h.admin_url, "http://127.0.0.1:9091");
        assert_eq!(h.proxy_url, "http://127.0.0.1:9090");
    }

    #[test]
    fn hackamore_section_defaults_to_absent() {
        let cfg: HorsieConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.hackamore.is_none());
        assert!(HorsieConfig::default().hackamore.is_none());
    }

    #[test]
    fn hackamore_section_missing_required_field_is_rejected() {
        // A hackamore section without an admin_url cannot mint — reject at parse
        // time rather than failing every spawn.
        let res =
            serde_json::from_str::<HorsieConfig>(r#"{ "hackamore": { "proxy_url": "http://p" } }"#);
        assert!(res.is_err());
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
    fn default_config_is_empty_but_valid() {
        let cfg = HorsieConfig::default();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());
        // State and data resolve to distinct dirs (different XDG bases / leaves).
        assert_ne!(cfg.storage.state_dir, cfg.storage.data_dir);
        assert!(cfg.sandbox.capabilities_file.is_none());
    }

    #[test]
    fn parses_config_with_no_providers_or_models() {
        // A file present but missing providers/models parses to empty maps.
        let cfg: HorsieConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());
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
        std::fs::write(
            &path,
            r#"{ "providers": {}, "models": { "m": { "provider": "p", "model_id": "id" } } }"#,
        )
        .unwrap();
        let cfg = HorsieConfig::resolve(Some(&path)).unwrap();
        assert!(cfg.models.contains_key("m"));
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
        std::fs::write(
            &path,
            r#"{ "models": { "u": { "provider": "p", "model_id": "id" } } }"#,
        )
        .unwrap();
        let cfg = HorsieConfig::resolve_with(None, Some(path)).unwrap();
        assert!(cfg.models.contains_key("u"));
    }

    #[test]
    fn resolve_with_defaults_when_user_config_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        // No flag and the user config does not exist → empty default config.
        let cfg = HorsieConfig::resolve_with(None, Some(missing)).unwrap();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());

        let cfg = HorsieConfig::resolve_with(None, None).unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn storage_and_sandbox_default_when_absent() {
        let json = r#"{
            "providers": { "m": { "type": "anthropic", "base_url": "http://localhost:1" } },
            "models": { "x": { "provider": "m", "model_id": "id" } }
        }"#;
        let cfg: HorsieConfig = serde_json::from_str(json).unwrap();
        assert_ne!(cfg.storage.state_dir, cfg.storage.data_dir);
        assert!(cfg.sandbox.capabilities_file.is_none());
        assert!(cfg.models["x"].max_tokens.is_none());
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
