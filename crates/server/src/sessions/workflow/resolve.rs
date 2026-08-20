//! Resolving a workflow definition into a self-contained run spec.
//!
//! A run snapshots everything at creation: every step's preset is flattened
//! into the step's own settings, so the driver never reaches a store and a
//! preset edited mid-run cannot change a step that has not started yet.
//!
//! Split in two so the decisions are testable without a database:
//! [`assemble_run_spec`] is the pure half — what a run *is*, given the
//! definition and the presets — and [`resolve_run_spec`] is the plumbing that
//! gathers those inputs from the stores. Both the HTTP `run` operation and the
//! `invoke_workflow` tool resolve through here, which is what keeps a run
//! created mid-session identical to one created by a request.

use crate::sessions::spec::AgentSettings;
use crate::sessions::workflow::{
    DEFAULT_MAX_STEPS, TransitionSpec, WorkflowRunSpec, WorkflowStepSpec, outcomes_or_default,
};
use crate::users::UserServices;
use crate::workflows::{WorkflowError, WorkflowRow};
use horsie_models::agents::AgentView;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Why a workflow could not be resolved into a run.
///
/// Its own type rather than `ControlError` because resolution is asked for from
/// two places — an HTTP request and an agent's tool call — and each renders a
/// refusal in its own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveRunError {
    /// No workflow by that name.
    NotFound(String),
    /// The definition cannot run as asked: empty input, a vanished preset, or
    /// a model no longer configured.
    Invalid(String),
    /// A backing store could not be read.
    Internal(String),
}

impl std::fmt::Display for ResolveRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Invalid(m) | Self::Internal(m) => f.write_str(m),
        }
    }
}

impl From<WorkflowError> for ResolveRunError {
    fn from(e: WorkflowError) -> Self {
        match e {
            WorkflowError::NotFound(m) => Self::NotFound(m),
            WorkflowError::Invalid(m) => Self::Invalid(m),
            WorkflowError::Conflict(m) | WorkflowError::Internal(m) => Self::Internal(m),
        }
    }
}

/// A run spec ready to start, and the union of its steps' plugin bundles.
///
/// The union is for the *session*: it is what the command catalogue is read
/// against, so `/commit` resolves whichever step declared it. It is not what
/// gets installed — each step provisions its own preset's bundles into its own
/// tree, so a step gets its own skills and not its siblings'.
#[derive(Debug, Clone)]
pub struct ResolvedRun {
    pub run: Arc<WorkflowRunSpec>,
    pub plugins: Vec<String>,
}

/// Resolve `name` into a run handed `input`, reading the stores once.
///
/// After this the run is self-contained; nothing about it re-reads a store.
pub async fn resolve_run_spec(
    services: &UserServices,
    name: &str,
    input: &str,
) -> Result<ResolvedRun, ResolveRunError> {
    // Also checked by assembly; repeated here so a caller with no input is
    // refused before any store is read, whether or not the workflow exists.
    if input.trim().is_empty() {
        return Err(ResolveRunError::Invalid(
            "input must not be empty — it is what the first step is handed".to_string(),
        ));
    }
    let row = services.workflows.row(name).await?;
    let view = services
        .config_store
        .view()
        .await
        .map_err(ResolveRunError::Internal)?;
    let models: Vec<String> = view.models.into_iter().map(|m| m.alias).collect();
    // Fetch what assembly will look up. A preset that cannot be read is simply
    // absent, and assembly names the step that needed it — the decision about
    // what "missing" means stays in the pure half.
    let mut presets = BTreeMap::new();
    for step in &row.steps {
        if presets.contains_key(&step.agent) {
            continue;
        }
        if let Ok(preset) = services.agents.get(&step.agent).await {
            presets.insert(step.agent.clone(), preset);
        }
    }
    assemble_run_spec(&row, &presets, &models, input)
}

/// The pure half: what a run of `row` is, given the presets and the configured
/// model aliases. Every refusal a definition can earn is decided here.
pub fn assemble_run_spec(
    row: &WorkflowRow,
    presets: &BTreeMap<String, AgentView>,
    configured_models: &[String],
    input: &str,
) -> Result<ResolvedRun, ResolveRunError> {
    if input.trim().is_empty() {
        return Err(ResolveRunError::Invalid(
            "input must not be empty — it is what the first step is handed".to_string(),
        ));
    }
    let mut steps = Vec::with_capacity(row.steps.len());
    let mut plugins: Vec<String> = Vec::new();
    for step in &row.steps {
        let preset = presets.get(&step.agent).ok_or_else(|| {
            ResolveRunError::Invalid(format!(
                "step '{}': agent preset '{}' no longer exists",
                step.name, step.agent
            ))
        })?;
        if !configured_models.contains(&preset.model) {
            return Err(ResolveRunError::Invalid(format!(
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
            settings: step_settings(preset, step.max_iterations, step.max_retries),
        });
    }
    let run = Arc::new(WorkflowRunSpec {
        workflow: row.name.clone(),
        start: row.start.clone(),
        steps,
        input: input.to_string(),
        // Snapshotted with the rest: raising a definition's budget must not
        // change a run already under way.
        max_steps: row.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
    });
    Ok(ResolvedRun { run, plugins })
}

/// One step's agent settings, flattened from its preset at run creation.
///
/// The decision, not the plumbing: what a step runs *as*. In particular
/// `plugins` is the step's own preset's, never the run's union — that is what
/// lets two steps of one workflow hold different skills.
fn step_settings(
    preset: &AgentView,
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::workflow::WorkflowStepDef;

    fn preset(name: &str, plugins: &[&str]) -> AgentView {
        AgentView {
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

    fn step(name: &str, agent: &str) -> WorkflowStepDef {
        WorkflowStepDef {
            name: name.into(),
            agent: agent.into(),
            prompt: "do it".into(),
            outcomes: None,
            fields: None,
            interactive: None,
            transitions: None,
            max_iterations: None,
            max_retries: None,
        }
    }

    fn row(steps: Vec<WorkflowStepDef>) -> WorkflowRow {
        WorkflowRow {
            name: "review".into(),
            description: String::new(),
            start: steps.first().map(|s| s.name.clone()).unwrap_or_default(),
            steps,
            max_steps: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn presets_of(list: &[AgentView]) -> BTreeMap<String, AgentView> {
        list.iter().map(|p| (p.name.clone(), p.clone())).collect()
    }

    #[test]
    fn empty_input_is_refused_before_anything_is_read() {
        let err = assemble_run_spec(&row(vec![]), &BTreeMap::new(), &[], "  \n")
            .expect_err("empty input must not resolve");
        match err {
            ResolveRunError::Invalid(m) => assert!(m.contains("input must not be empty"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_vanished_preset_names_the_step_that_needed_it() {
        let r = row(vec![step("plan", "architect")]);
        let err = assemble_run_spec(&r, &BTreeMap::new(), &["sonnet".into()], "go")
            .expect_err("a missing preset must not resolve");
        match err {
            ResolveRunError::Invalid(m) => {
                assert!(m.contains("step 'plan'"), "{m}");
                assert!(m.contains("'architect' no longer exists"), "{m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn an_unconfigured_model_names_step_and_model() {
        let r = row(vec![step("plan", "architect")]);
        let presets = presets_of(&[preset("architect", &[])]);
        let err = assemble_run_spec(&r, &presets, &["opus".into()], "go")
            .expect_err("an unconfigured model must not resolve");
        match err {
            ResolveRunError::Invalid(m) => {
                assert!(m.contains("step 'plan'"), "{m}");
                assert!(m.contains("model 'sonnet'"), "{m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_resolved_run_snapshots_the_definition() {
        let mut second = step("verify", "reviewer");
        second.max_iterations = Some(7);
        let r = row(vec![step("plan", "architect"), second]);
        let presets = presets_of(&[preset("architect", &["a-kit"]), preset("reviewer", &[])]);
        let resolved = assemble_run_spec(&r, &presets, &["sonnet".into()], "go").unwrap();
        assert_eq!(resolved.run.workflow, "review");
        assert_eq!(resolved.run.start, "plan");
        assert_eq!(resolved.run.input, "go");
        assert_eq!(resolved.run.max_steps, DEFAULT_MAX_STEPS);
        let verify = resolved.run.step("verify").unwrap();
        assert_eq!(verify.settings.max_iterations, Some(7));
        // Outcomes are defaulted into the snapshot, not re-derived at read time.
        assert!(!verify.outcomes.is_empty());
    }

    /// The union serves the session's command catalogue; each step's settings
    /// keep only its own preset's bundles (#182).
    #[test]
    fn the_plugin_union_spans_steps_but_settings_stay_per_step() {
        let r = row(vec![step("plan", "architect"), step("verify", "reviewer")]);
        let presets = presets_of(&[
            preset("architect", &["superpowers", "docs-kit"]),
            preset("reviewer", &["docs-kit", "review-kit"]),
        ]);
        let resolved = assemble_run_spec(&r, &presets, &["sonnet".into()], "go").unwrap();
        assert_eq!(resolved.plugins, ["superpowers", "docs-kit", "review-kit"]);
        assert_eq!(
            resolved.run.step("plan").unwrap().settings.plugins,
            ["superpowers", "docs-kit"]
        );
        assert_eq!(
            resolved.run.step("verify").unwrap().settings.plugins,
            ["docs-kit", "review-kit"]
        );
    }

    #[test]
    fn a_step_whose_preset_selects_no_bundles_gets_none() {
        let r = row(vec![step("plan", "plain")]);
        let presets = presets_of(&[preset("plain", &[])]);
        let resolved = assemble_run_spec(&r, &presets, &["sonnet".into()], "go").unwrap();
        assert!(
            resolved
                .run
                .step("plan")
                .unwrap()
                .settings
                .plugins
                .is_empty()
        );
        assert!(resolved.plugins.is_empty());
    }

    #[test]
    fn workflow_store_errors_map_onto_resolution_errors() {
        assert_eq!(
            ResolveRunError::from(WorkflowError::NotFound("no such workflow".into())),
            ResolveRunError::NotFound("no such workflow".into())
        );
        assert_eq!(
            ResolveRunError::from(WorkflowError::Internal("db gone".into())),
            ResolveRunError::Internal("db gone".into())
        );
    }
}
