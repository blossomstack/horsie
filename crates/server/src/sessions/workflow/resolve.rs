//! Resolving a workflow name into a self-contained run snapshot.
//!
//! This is the half of "start a run" that reads stores: the definition row,
//! each step's agent preset, and the configured models. It lives here rather
//! than in `control` because more than one caller needs it — the HTTP `run`
//! operation today, and an agent invoking a workflow mid-session tomorrow.
//! After it returns, the run is self-contained: the driver never reaches a
//! store, and a preset edited mid-run cannot change a step that has not
//! started yet.

use crate::sessions::spec::AgentSettings;
use crate::sessions::workflow::{
    DEFAULT_MAX_STEPS, TransitionSpec, WorkflowRunSpec, WorkflowStepSpec, outcomes_or_default,
};
use crate::users::UserServices;
use crate::workflows::WorkflowError;
use std::sync::Arc;

/// Why a workflow name could not become a run snapshot.
///
/// Deliberately not `ControlError`: the session layer must not depend on the
/// control surface, and an agent's tool call renders these as tool errors, not
/// HTTP statuses.
#[derive(Debug)]
pub enum ResolveError {
    /// No workflow by that name.
    NotFound(String),
    /// The definition cannot run as saved: empty input, a preset that no
    /// longer exists, a model no longer configured.
    Invalid(String),
    /// A store failed.
    Internal(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Invalid(m) | Self::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<WorkflowError> for ResolveError {
    fn from(e: WorkflowError) -> Self {
        match e {
            WorkflowError::NotFound(m) => Self::NotFound(m),
            WorkflowError::Invalid(m) | WorkflowError::Conflict(m) => Self::Invalid(m),
            WorkflowError::Internal(m) => Self::Internal(m),
        }
    }
}

/// A run snapshot, plus the one session-scoped fact resolution discovers.
pub struct ResolvedRun {
    pub run: Arc<WorkflowRunSpec>,
    /// The union of every step's plugin bundles, in first-seen order. It is
    /// the command-catalogue scope for a session created from this run — and
    /// nothing else: each step installs its own preset's bundles, never this
    /// union. A run invoked inside a live session has no use for it.
    pub plugins: Vec<String>,
}

/// One step's agent settings, flattened from its preset at run creation.
///
/// This is the decision, not the plumbing: what a step runs *as*. In
/// particular `plugins` is the step's own preset's, never the run's union —
/// that is what lets two steps of one workflow hold different skills, and it
/// is worth a test that needs no database behind it.
pub fn step_settings(
    preset: &horsie_models::agents::AgentView,
    max_iterations: Option<u32>,
    max_retries: Option<u32>,
) -> AgentSettings {
    AgentSettings {
        model: preset.model.clone(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations,
        max_retries: max_retries.unwrap_or(0),
        mcp_servers: preset.mcp_servers.clone(),
        memory_spaces: preset.memory_spaces.clone(),
        thinking_effort: preset.thinking_effort.clone(),
        max_concurrent_subagents: None,
        instructions: preset.instructions.clone(),
        auto_compact: preset.auto_compact,
        // A workflow step is not a main agent, and only a main agent gets the
        // control-plane tools.
        control_plane: None,
        // This step's own bundles, never the run's union. Installed into this
        // step's own tree on the shared runtime.
        plugins: preset.plugins.clone(),
    }
}

/// Resolve `name` into a run snapshot handed `input` as its first step's
/// input. Every step's preset is resolved once, here.
pub async fn resolve_run_spec(
    services: &UserServices,
    name: &str,
    input: &str,
) -> Result<ResolvedRun, ResolveError> {
    if input.trim().is_empty() {
        return Err(ResolveError::Invalid(
            "input must not be empty — it is what the first step is handed".to_string(),
        ));
    }
    let row = services.workflows.row(name).await?;
    let view = services
        .config_store
        .view()
        .await
        .map_err(ResolveError::Internal)?;

    let mut steps = Vec::with_capacity(row.steps.len());
    let mut plugins: Vec<String> = Vec::new();
    for step in &row.steps {
        let preset = services.agents.get(&step.agent).await.map_err(|_| {
            ResolveError::Invalid(format!(
                "step '{}': agent preset '{}' no longer exists",
                step.name, step.agent
            ))
        })?;
        if !view.models.iter().any(|m| m.alias == preset.model) {
            return Err(ResolveError::Invalid(format!(
                "step '{}': model '{}' is no longer configured",
                step.name, preset.model
            )));
        }
        for p in &preset.plugins {
            if !plugins.contains(p) {
                plugins.push(p.clone());
            }
        }
        steps.push(WorkflowStepSpec {
            name: step.name.clone(),
            agent: step.agent.clone(),
            prompt: step.prompt.clone(),
            // Defaulted here rather than at read time: the snapshot is what a
            // run answers from, so it should not have to re-derive anything.
            outcomes: outcomes_or_default(step.outcomes.as_ref()),
            fields: step.fields.clone().unwrap_or_default(),
            interactive: step.interactive.unwrap_or(false),
            transitions: step
                .transitions
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|t| TransitionSpec {
                    to: t.to,
                    when: t.when,
                })
                .collect(),
            settings: step_settings(&preset, step.max_iterations, step.max_retries),
        });
    }
    let run = Arc::new(WorkflowRunSpec {
        workflow: name.to_string(),
        start: row.start.clone(),
        steps,
        input: input.to_string(),
        // Snapshotted with the rest: raising a definition's budget must not
        // change a run already under way.
        max_steps: row.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
    });
    Ok(ResolvedRun { run, plugins })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn preset(name: &str, plugins: &[&str]) -> horsie_models::agents::AgentView {
        horsie_models::agents::AgentView {
            name: name.into(),
            description: String::new(),
            instructions: None,
            model: "sonnet".into(),
            plugins: plugins.iter().map(|p| (*p).to_string()).collect(),
            mcp_servers: Vec::new(),
            memory_spaces: Vec::new(),
            thinking_effort: None,
            auto_compact: None,
            control_plane: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// The point of per-agent provisioning, and what #182 tracked as a known
    /// limitation: two steps of one workflow run with their own skills.
    ///
    /// Every step used to be handed the run's union — the bundle manifest was
    /// written once into the runtime's environment — so a step got its
    /// siblings' skills as well as its own, and could never be given fewer.
    #[test]
    fn each_step_carries_its_own_presets_bundles_and_not_its_siblings() {
        let reviewer = step_settings(&preset("reviewer", &["superpowers"]), None, None);
        let writer = step_settings(&preset("writer", &["docs-kit"]), None, None);

        assert_eq!(reviewer.plugins, vec!["superpowers".to_string()]);
        assert_eq!(writer.plugins, vec!["docs-kit".to_string()]);
        assert!(
            !writer.plugins.contains(&"superpowers".to_string()),
            "a step must not inherit a sibling's bundles"
        );
    }

    /// A preset that selects nothing gets nothing — not the union, and not the
    /// other steps' sets. The empty case is what the account default-enabled
    /// fallback is resolved against later, per agent.
    #[test]
    fn a_step_whose_preset_selects_no_bundles_gets_none() {
        assert!(
            step_settings(&preset("plain", &[]), None, None)
                .plugins
                .is_empty()
        );
    }
}
