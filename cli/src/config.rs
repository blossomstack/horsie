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
/// All fields default, so `OctoberConfig::default()` is a valid empty config
/// (no providers, no models, default storage/sandbox). An empty config is a
/// legal state — `validate` is what rejects workflows that reference models
/// the config doesn't define.
#[derive(Debug, Default, Deserialize)]
pub struct OctoberConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub storage: StorageConfig,
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
    pub root_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("./.october"),
        }
    }
}

impl OctoberConfig {
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
    }

    /// Resolve the config per CLI policy:
    /// - `explicit` path given (the `--config` flag) → load it; a missing or
    ///   malformed file is an error, since the user asked for it by name.
    /// - no flag → load the user config at [`user_config_path`] if it exists;
    ///   otherwise fall back to an empty [`OctoberConfig::default`].
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

/// The default user config path, `<config-dir>/october/config.json`, where
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
    Some(config_dir.join("october").join("config.json"))
}

/// Build the provider registry keyed by **model key** (matches `WorkflowAgentDef.model`).
/// The key is resolved inline-then-env-then-none; a configured-but-missing/empty key
/// fails here, before any runtime is spawned.
pub fn build_registry(
    cfg: &OctoberConfig,
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
            "storage": { "root_dir": "./.october" }
        }"#;
        let cfg: OctoberConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.providers.contains_key("anthropic"));
        assert_eq!(cfg.models["sonnet"].model_id, "claude-sonnet-4-6");
        assert_eq!(cfg.storage.root_dir, PathBuf::from("./.october"));
    }

    #[test]
    fn inline_api_key_builds_registry_without_env() {
        // Inline key path needs no env var and no network — just constructs providers.
        let cfg: OctoberConfig = serde_json::from_str(
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
        let cfg: OctoberConfig = serde_json::from_str(
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
        let cfg: OctoberConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } },
                "sandbox": { "capabilities_file": "/etc/october/caps.json" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            cfg.sandbox.capabilities_file,
            Some(PathBuf::from("/etc/october/caps.json"))
        );
    }

    #[test]
    fn capabilities_file_defaults_to_none() {
        let cfg: OctoberConfig = serde_json::from_str(
            r#"{
                "providers": { "p": { "type": "anthropic", "base_url": "http://localhost:1" } },
                "models": { "m": { "provider": "p", "model_id": "id" } }
            }"#,
        )
        .unwrap();
        assert!(cfg.sandbox.capabilities_file.is_none());
    }

    #[test]
    fn default_config_is_empty_but_valid() {
        let cfg = OctoberConfig::default();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());
        assert_eq!(cfg.storage.root_dir, PathBuf::from("./.october"));
        assert!(cfg.sandbox.capabilities_file.is_none());
    }

    #[test]
    fn parses_config_with_no_providers_or_models() {
        // A file present but missing providers/models parses to empty maps.
        let cfg: OctoberConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn user_config_path_prefers_xdg() {
        let p = user_config_path_from(Some("/xdg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/xdg/october/config.json")));
    }

    #[test]
    fn user_config_path_falls_back_to_home_dot_config() {
        // Unset and empty XDG both fall through to $HOME/.config.
        for xdg in [None, Some("".into())] {
            let p = user_config_path_from(xdg, Some("/home/u".into()));
            assert_eq!(
                p,
                Some(PathBuf::from("/home/u/.config/october/config.json"))
            );
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
        let cfg = OctoberConfig::resolve(Some(&path)).unwrap();
        assert!(cfg.models.contains_key("m"));
    }

    #[test]
    fn resolve_errors_on_missing_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(OctoberConfig::resolve(Some(&missing)).is_err());
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
        let cfg = OctoberConfig::resolve_with(None, Some(path)).unwrap();
        assert!(cfg.models.contains_key("u"));
    }

    #[test]
    fn resolve_with_defaults_when_user_config_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        // No flag and the user config does not exist → empty default config.
        let cfg = OctoberConfig::resolve_with(None, Some(missing)).unwrap();
        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());

        let cfg = OctoberConfig::resolve_with(None, None).unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn storage_and_sandbox_default_when_absent() {
        let json = r#"{
            "providers": { "m": { "type": "anthropic", "base_url": "http://localhost:1" } },
            "models": { "x": { "provider": "m", "model_id": "id" } }
        }"#;
        let cfg: OctoberConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.storage.root_dir, PathBuf::from("./.october"));
        assert!(cfg.sandbox.capabilities_file.is_none());
        assert!(cfg.models["x"].max_tokens.is_none());
    }
}
