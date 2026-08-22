//! Assembling a [`SessionSpec`] from the pieces every creation path supplies.
//!
//! Every way a session comes into existence — the sessions API, an agent-preset
//! invoke, a routine trigger — goes through here, so the three can never drift
//! on provisioning, plugin, or thinking-effort semantics. It lives beside the
//! session types rather than in the HTTP layer because a routine's timer is not
//! an HTTP caller.

use crate::config::ConfigStore;
use crate::environments::{EnvironmentError, EnvironmentService};
use crate::sessions::spec::{
    AgentSettings, EnvVarSpec, ProvisionStepSpec, SessionKind, SessionOrigin, SessionSpec,
    WorkspaceDef,
};
use horsie_models::environments::EnvironmentSpec;
use horsie_models::session::AgentSettings as WireAgentSettings;
use std::sync::Arc;

/// Why a spec could not be assembled. Split so callers pick a status without
/// string matching: `Invalid` is the caller's fault, `Internal` is ours.
#[derive(Debug)]
pub enum SpecError {
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SpecError {}

/// Storage `AgentSettings` from the wire request, applying defaults.
fn settings_from_wire(w: WireAgentSettings) -> AgentSettings {
    AgentSettings {
        model: w.model,
        allowed_tools: w.allowed_tools,
        use_plugins: w.use_plugins,
        max_iterations: w.max_iterations,
        max_retries: w.max_retries.unwrap_or(0),
        mcp_servers: w.mcp_servers.unwrap_or_default(),
        memory_spaces: w.memory_spaces.unwrap_or_default(),
        thinking_effort: w.thinking_effort,
        max_concurrent_subagents: w.max_concurrent_subagents,
        instructions: w.instructions,
        auto_compact: w.auto_compact,
        plugins: Vec::new(),
        // Filled in by `build_session_spec` once the session's bundle names
        // are resolved; the wire request names them beside the agent, not
        // inside it.
    }
}

/// The session-shaped half of a spec, shared by every creation path: the
/// environment/provision facts and provenance, with nothing about which agent
/// or workflow runs in it.
struct CommonSpec {
    workspaces: Vec<WorkspaceDef>,
    provision: Vec<ProvisionStepSpec>,
    vendor: String,
    plugins: Vec<String>,
    origin: SessionOrigin,
    environment: Option<String>,
    env_vars: Vec<EnvVarSpec>,
}

/// Resolve the environment and provenance facts every spec carries, once,
/// snapshotted into the spec below. `RuntimeManager::runtime_spec` re-reads
/// that snapshot on every create *and* every revive, so a session revived next
/// week gets what it was created with rather than what a since-edited
/// environment now says.
async fn resolve_common(
    environments: &EnvironmentService,
    environment: EnvironmentSpec,
    plugins: Option<Vec<String>>,
    origin: SessionOrigin,
) -> Result<CommonSpec, SpecError> {
    let environment_name = match &environment {
        EnvironmentSpec::Named(n) => Some(n.name.clone()),
        EnvironmentSpec::Runtime(_) => None,
    };
    let (vendor, repos, env_vars, setup) = match environment {
        EnvironmentSpec::Runtime(r) => (r.vendor, r.repos.unwrap_or_default(), vec![], vec![]),
        EnvironmentSpec::Named(n) => {
            let env = environments.get(&n.name).await.map_err(|e| match e {
                // An unknown or unusable name is what the caller asked for, so
                // it is their 422 — not our 500.
                EnvironmentError::NotFound(m) | EnvironmentError::Invalid(m) => {
                    SpecError::Invalid(m)
                }
                EnvironmentError::Conflict(m) | EnvironmentError::Internal(m) => {
                    SpecError::Internal(m)
                }
            })?;
            (env.vendor, env.repos, env.env_vars, env.provision)
        }
    };
    if vendor.trim().is_empty() {
        return Err(SpecError::Invalid(
            "environment names no runtime vendor".to_string(),
        ));
    }
    // The workspace is always vendor-allocated; `repos` (when the vendor
    // supports provisioning) become git-checkout provision steps that clone
    // into it. The UI only sends repos to a provisioning-capable vendor; a
    // vendor that can't provision rejects them at `create()`.
    //
    // Checkouts run before the environment's own steps: a step like
    // `make setup` needs its repo to be on disk already.
    let provision: Vec<ProvisionStepSpec> = horsie_models::provision_from_repos(&repos)
        .map_err(|e| SpecError::Invalid(format!("invalid repos: {e}")))?
        .into_iter()
        .chain(setup)
        .map(|s| ProvisionStepSpec {
            name: s.name,
            uses: s.uses,
            with: s.with.into_iter().map(|p| (p.key, p.value)).collect(),
        })
        .collect();
    let env_vars: Vec<EnvVarSpec> = env_vars
        .into_iter()
        .map(|v| EnvVarSpec {
            name: v.name,
            value: v.value,
        })
        .collect();
    let workspaces = vec![WorkspaceDef {
        name: "main".into(),
    }];
    Ok(CommonSpec {
        workspaces,
        provision,
        vendor,
        plugins: plugins.unwrap_or_default(),
        origin,
        environment: environment_name,
        env_vars,
    })
}

/// Assemble an agent session's [`SessionSpec`], resolving defaults and
/// validating everything that is knowable before the session exists.
pub async fn build_session_spec(
    config: &Arc<dyn ConfigStore>,
    environments: &EnvironmentService,
    agent: WireAgentSettings,
    environment: EnvironmentSpec,
    plugins: Option<Vec<String>>,
    origin: SessionOrigin,
) -> Result<SessionSpec, SpecError> {
    let common = resolve_common(environments, environment, plugins, origin).await?;
    let mut agent = settings_from_wire(agent);
    // Selected bundle names (empty → the provisioner falls back to the
    // default-enabled set). Selecting bundles implies plugins are surfaced, so
    // force the agent's opt-in when any are chosen.
    if !common.plugins.is_empty() {
        agent.use_plugins = Some(true);
    }
    // The session's main agent runs with the session's bundles. Held on the
    // agent as well as on the spec because provisioning is per agent now, and
    // the two readers want different things: the spec's copy is the union the
    // command catalogue is read against, the agent's is what gets installed.
    agent.plugins.clone_from(&common.plugins);
    // Resolve the effective thinking effort once, here: session choice wins,
    // else the model's configured default, else nothing. Effort is fixed for a
    // session's lifetime (changing it mid-session invalidates the prompt
    // cache), so freezing it at creation is deliberate. A requested value must
    // be canonical AND offered by the model — otherwise it reaches the provider
    // as an opaque 400.
    {
        let model_row = config
            .view()
            .await
            .map_err(SpecError::Internal)?
            .models
            .into_iter()
            .find(|m| m.alias == agent.model);
        match agent.thinking_effort.as_deref() {
            Some(requested) => {
                let effort =
                    horsie_agentcore::ThinkingEffort::parse(requested).ok_or_else(|| {
                        SpecError::Invalid(format!("unknown thinking effort '{requested}'"))
                    })?;
                let offered = model_row
                    .as_ref()
                    .and_then(|m| m.thinking_efforts.clone())
                    .unwrap_or_default();
                if !offered.iter().any(|e| e == effort.as_str()) {
                    return Err(SpecError::Invalid(format!(
                        "model '{}' does not offer thinking effort '{requested}'",
                        agent.model
                    )));
                }
            }
            None => {
                agent.thinking_effort = model_row.and_then(|m| m.thinking_effort);
            }
        }
    }
    Ok(SessionSpec {
        kind: SessionKind::Agent { settings: agent },
        workspaces: common.workspaces,
        provision: common.provision,
        vendor: common.vendor,
        plugins: common.plugins,
        origin: common.origin,
        environment: common.environment,
        env_vars: common.env_vars,
    })
}

/// Assemble a workflow run's [`SessionSpec`]: the same shared environment and
/// provision facts, with the run snapshot as its kind. No `AgentSettings` is
/// fabricated — the steps own their own, and nothing session-shaped needs one.
pub async fn build_workflow_spec(
    environments: &EnvironmentService,
    environment: EnvironmentSpec,
    plugins: Vec<String>,
    run: Arc<crate::sessions::workflow::WorkflowRunSpec>,
) -> Result<SessionSpec, SpecError> {
    let common = resolve_common(
        environments,
        environment,
        Some(plugins),
        SessionOrigin::User,
    )
    .await?;
    Ok(SessionSpec {
        kind: SessionKind::Workflow { run },
        workspaces: common.workspaces,
        provision: common.provision,
        vendor: common.vendor,
        plugins: common.plugins,
        origin: common.origin,
        environment: common.environment,
        env_vars: common.env_vars,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::environments::{EnvironmentInput, NamedEnvironment, RuntimeEnvironment};
    use horsie_models::executor::{EnvVar, ProvisionStep, StepParam};
    use horsie_models::session_api::RepoConfig;
    use horsie_models::settings::{ModelInput, ProviderInput};

    /// A config store carrying one model ("m"), and an environment service on
    /// the same database.
    async fn fixtures() -> (Arc<dyn ConfigStore>, EnvironmentService) {
        let db = crate::db::testing::db().await;
        let opened = crate::config::DbConfigStore::open_on(
            db.clone(),
            crate::config::StoreDeps {
                info: horsie_models::settings::ServerInfo {
                    config_path: String::new(),
                    database: String::new(),
                    state_dir: String::new(),
                    data_dir: String::new(),
                    plugins_dir: String::new(),
                    version: "test".into(),
                },
            },
            crate::projects::ProjectId::new("1"),
        )
        .await
        .unwrap();
        opened
            .store
            .seed(
                vec![ProviderInput {
                    name: "p".into(),
                    kind: "anthropic".into(),
                    base_url: Some("http://localhost:1".into()),
                    api_key: Some("sk-x".into()),
                    keep_thinking_signature: None,
                }],
                vec![ModelInput {
                    alias: "m".into(),
                    provider: "p".into(),
                    model_id: "claude".into(),
                    max_tokens: None,
                    context_window: None,
                    thinking_efforts: None,
                    thinking_effort: None,
                    thinking_dialect: None,
                    forced_tools_disable_thinking: None,
                }],
            )
            .await
            .unwrap();
        let envs = EnvironmentService::new(crate::environments::EnvironmentStore::new(
            db,
            crate::projects::ProjectId::new("1"),
        ));
        (opened.store as Arc<dyn ConfigStore>, envs)
    }

    fn wire_settings() -> WireAgentSettings {
        WireAgentSettings {
            model: "m".into(),
            instructions: None,
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: None,
            mcp_servers: None,
            memory_spaces: None,
            thinking_effort: None,
            max_concurrent_subagents: None,
            auto_compact: None,
        }
    }

    fn repo(url: &str) -> RepoConfig {
        RepoConfig {
            url: url.into(),
            git_ref: None,
            dir: None,
        }
    }

    async fn build(
        config: &Arc<dyn ConfigStore>,
        envs: &EnvironmentService,
        environment: EnvironmentSpec,
    ) -> Result<SessionSpec, SpecError> {
        build_session_spec(
            config,
            envs,
            wire_settings(),
            environment,
            None,
            SessionOrigin::User,
        )
        .await
    }

    #[tokio::test]
    async fn an_ad_hoc_environment_sets_the_vendor_and_clones_its_repos() {
        let (config, envs) = fixtures().await;
        let spec = build(
            &config,
            &envs,
            EnvironmentSpec::Runtime(RuntimeEnvironment {
                vendor: "fly".into(),
                repos: Some(vec![repo("https://github.com/o/api")]),
            }),
        )
        .await
        .unwrap();
        assert_eq!(spec.vendor, "fly");
        // Nothing predefined was named, so there is no provenance to record.
        assert_eq!(spec.environment, None);
        assert_eq!(spec.provision.len(), 1);
        assert_eq!(spec.provision[0].uses, "git_checkout");
        assert!(spec.env_vars.is_empty());
    }

    #[tokio::test]
    async fn a_named_environment_contributes_everything_it_has() {
        let (config, envs) = fixtures().await;
        envs.create(EnvironmentInput {
            name: "staging".into(),
            description: None,
            vendor: "fly".into(),
            repos: Some(vec![repo("https://github.com/o/api")]),
            env_vars: Some(vec![EnvVar {
                name: "RUST_LOG".into(),
                value: "debug".into(),
            }]),
            provision: Some(vec![ProvisionStep {
                name: "setup".into(),
                uses: "run".into(),
                with: vec![StepParam {
                    key: "cmd".into(),
                    value: "make setup".into(),
                }],
            }]),
        })
        .await
        .unwrap();
        let spec = build(
            &config,
            &envs,
            EnvironmentSpec::Named(NamedEnvironment {
                name: "staging".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(spec.vendor, "fly");
        assert_eq!(spec.environment.as_deref(), Some("staging"));
        // The checkout first: `make setup` needs the repo to be there.
        assert_eq!(spec.provision[0].uses, "git_checkout");
        assert_eq!(spec.provision[1].uses, "run");
        assert_eq!(spec.env_vars[0].name, "RUST_LOG");
        assert_eq!(spec.env_vars[0].value, "debug");
    }

    #[tokio::test]
    async fn an_unknown_environment_is_the_callers_fault() {
        let (config, envs) = fixtures().await;
        let err = build(
            &config,
            &envs,
            EnvironmentSpec::Named(NamedEnvironment {
                name: "ghost".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, SpecError::Invalid(ref m) if m.contains("ghost")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_ad_hoc_environment_must_name_a_vendor() {
        let (config, envs) = fixtures().await;
        let err = build(
            &config,
            &envs,
            EnvironmentSpec::Runtime(RuntimeEnvironment {
                vendor: "  ".into(),
                repos: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SpecError::Invalid(_)), "{err}");
    }
}
