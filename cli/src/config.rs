use crate::error::CliError;
use agentcore::LlmProvider;
use anthropic::AnthropicProvider;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// CLI-owned policy (hand-written serde — NOT a fluorite protocol type). The
/// workflow file stays a pure `WorkflowDefinition`, reusable across server/CLI.
#[derive(Debug, Deserialize)]
pub struct OctoberConfig {
    pub providers: HashMap<String, ProviderConfig>,
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
