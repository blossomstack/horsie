//! Assembling a [`SessionSpec`] from the pieces every creation path supplies.
//!
//! Every way a session comes into existence — the sessions API, an agent-preset
//! invoke, a routine trigger — goes through here, so the three can never drift
//! on provisioning, plugin, or thinking-effort semantics. It lives beside the
//! session types rather than in the HTTP layer because a routine's timer is not
//! an HTTP caller.

use crate::config::ConfigStore;
use crate::sessions::spec::{
    AgentSettings, ProvisionStepSpec, SessionOrigin, SessionSpec, WorkspaceDef,
};
use horsie_models::session::AgentSettings as WireAgentSettings;
use horsie_models::session_api::RepoConfig;
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
    }
}

/// Assemble a [`SessionSpec`], resolving defaults and validating everything
/// that is knowable before the session exists.
pub async fn build_session_spec(
    config: &Arc<dyn ConfigStore>,
    name: Option<String>,
    agent: WireAgentSettings,
    vendor: Option<String>,
    repos: Vec<RepoConfig>,
    plugins: Option<Vec<String>>,
    origin: SessionOrigin,
) -> Result<SessionSpec, SpecError> {
    // The workspace is always vendor-allocated; `repos` (when the vendor
    // supports provisioning) become git-checkout provision steps that clone
    // into it. The UI only sends repos to a provisioning-capable vendor; a
    // vendor that can't provision rejects them at `create()`.
    let provision: Vec<ProvisionStepSpec> = horsie_models::provision_from_repos(&repos)
        .map_err(|e| SpecError::Invalid(format!("invalid repos: {e}")))?
        .into_iter()
        .map(|s| ProvisionStepSpec {
            name: s.name,
            uses: s.uses,
            with: s.with.into_iter().map(|p| (p.key, p.value)).collect(),
        })
        .collect();
    let workspaces = vec![WorkspaceDef {
        name: "main".into(),
    }];
    // Selected bundle names (empty → the provisioner falls back to the
    // default-enabled set). Selecting bundles implies plugins are surfaced, so
    // force the agent's opt-in when any are chosen.
    let plugins = plugins.unwrap_or_default();
    let mut agent = settings_from_wire(agent);
    if !plugins.is_empty() {
        agent.use_plugins = Some(true);
    }
    // Resolve the effective thinking effort once, here: session choice wins,
    // else the model's configured default, else nothing. Effort is fixed for a
    // session's lifetime (changing it mid-conversation invalidates the prompt
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
        name,
        agent,
        workspaces,
        provision,
        vendor: vendor.unwrap_or_else(|| config.default_vendor()),
        plugins,
        origin,
        workflow: None,
    })
}
