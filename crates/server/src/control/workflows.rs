//! The workflows resource: a named graph of steps, and starting a run of one.
//!
//! A *run* is a session, so `run` here creates one. The run's graph projection
//! (`GET /api/sessions/{id}/workflow`) and its retry stay in `http::workflows`:
//! both hang off a session rather than a workflow, and they belong with the
//! session resource whenever that moves.

use crate::control::{
    ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, ask, op,
};
use crate::http::handlers;
use crate::sessions::builder::build_workflow_spec;
use crate::sessions::spec::{AgentSettings, SessionStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use crate::sessions::workflow::{
    DEFAULT_MAX_STEPS, TransitionSpec, WorkflowRunSpec, WorkflowStepSpec,
};
use crate::users::UserServices;
use horsie_models::now_ms;
use horsie_models::workflow::{
    WorkflowInput, WorkflowRunRequest, WorkflowRunResponse, WorkflowView,
};
use std::sync::Arc;

/// Seconds since the epoch, the stamp both `agents` and `routines` store.
fn now_secs() -> u64 {
    now_ms() / 1_000
}

/// `run` takes its slug from the path and the rest from the body.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RunWorkflow {
    /// Slug of the workflow to run.
    pub name: String,
    #[serde(flatten)]
    pub request: WorkflowRunRequest,
}

/// A named graph of steps, and starting a run of one.
pub struct Workflows;

impl Resource for Workflows {
    fn name(&self) -> &'static str {
        "workflows"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/workflows",
                "Every saved workflow definition.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    Ok::<Vec<WorkflowView>, ControlError>(s.workflows.list().await?)
                },
            ),
            op(
                "get",
                Method::Get,
                "/api/workflows/{name}",
                "One workflow definition by slug.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    Ok::<WorkflowView, ControlError>(s.workflows.get(&i.name).await?)
                },
            ),
            op(
                "create",
                Method::Post,
                "/api/workflows",
                "Save a new workflow: a graph of steps, each an agent preset plus a \
             fixed prompt, wired by conditions over the step's output.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: WorkflowInput| async move {
                    Ok::<WorkflowView, ControlError>(s.workflows.create(i, now_secs()).await?)
                },
            )
            .created(),
            op(
                "replace",
                Method::Put,
                "/api/workflows/{name}",
                "Replace a workflow wholesale. Runs already under way keep the \
             definition they snapshotted, so this never changes one.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: WorkflowInput| async move {
                    let name = i.name.clone();
                    Ok::<WorkflowView, ControlError>(
                        s.workflows.replace(&name, i, now_secs()).await?,
                    )
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/api/workflows/{name}",
                "Delete a workflow. Its past runs are ordinary sessions and stay \
             readable.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    s.workflows.delete(&i.name).await?;
                    Ok::<(), ControlError>(())
                },
            )
            .no_content(),
            op(
                "run",
                Method::Post,
                "/api/workflows/{name}/runs",
                "Start a run: creates the session that drives the graph and returns \
             it immediately. The first step begins on its own.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: RunWorkflow| async move { run_workflow(&s, i).await },
            )
            .created(),
        ]
    }
}

async fn run_workflow(
    services: &UserServices,
    input: RunWorkflow,
) -> Result<WorkflowRunResponse, ControlError> {
    let RunWorkflow { name, request: req } = input;
    if req.input.trim().is_empty() {
        return Err(ControlError::Invalid(
            "input must not be empty — it is what the first step is handed".to_string(),
        ));
    }
    let row = services.workflows.row(&name).await?;
    let view = services
        .config_store
        .view()
        .await
        .map_err(ControlError::Internal)?;

    // Resolve every step's preset once, here. After this the run is
    // self-contained: the driver never reaches a store, and a preset edited
    // mid-run cannot change a step that has not started yet.
    let mut steps = Vec::with_capacity(row.steps.len());
    let mut plugins: Vec<String> = Vec::new();
    for step in &row.steps {
        let preset = services.agents.get(&step.agent).await.map_err(|_| {
            ControlError::Invalid(format!(
                "step '{}': agent preset '{}' no longer exists",
                step.name, step.agent
            ))
        })?;
        if !view.models.iter().any(|m| m.alias == preset.model) {
            return Err(ControlError::Invalid(format!(
                "step '{}': model '{}' is no longer configured",
                step.name, preset.model
            )));
        }
        // One runtime is shared by every step and its bundle manifest is
        // written once at provision, so the run carries the union of what the
        // steps ask for. Tracked as a known limitation in #182.
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
            outcomes: crate::sessions::workflow::outcomes_or_default(step.outcomes.as_ref()),
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
            settings: AgentSettings {
                model: preset.model.clone(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: step.max_iterations,
                max_retries: step.max_retries.unwrap_or(0),
                mcp_servers: preset.mcp_servers.clone(),
                memory_spaces: preset.memory_spaces.clone(),
                thinking_effort: preset.thinking_effort.clone(),
                max_concurrent_subagents: None,
                instructions: preset.instructions.clone(),
                auto_compact: preset.auto_compact,
                // A workflow step is not a main agent, and only a main
                // agent gets the control-plane tools.
                control_plane: None,
            },
        });
    }
    let run = Arc::new(WorkflowRunSpec {
        workflow: name.clone(),
        start: row.start.clone(),
        steps,
        input: req.input.clone(),
        // Snapshotted with the rest: raising a definition's budget must not
        // change a run already under way.
        max_steps: row.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
    });
    // No `AgentSettings` is fabricated for the session: a run's agents are its
    // steps, each with its own settings, and nothing session-shaped needs one.
    let spec = build_workflow_spec(
        &services.environments,
        req.name.or_else(|| Some(name.clone())),
        req.environment,
        plugins,
        run,
    )
    .await?;
    // Checked on the *resolved* vendor: a named environment carries its own,
    // so there is nothing to check until the environment has been read.
    if !services
        .connected_vendors
        .connected_names()
        .contains(&spec.vendor)
    {
        return Err(ControlError::Invalid(format!(
            "runtime vendor '{}' is not connected",
            spec.vendor
        )));
    }

    let created_at = now_ms();
    // Creating it is enough to start it: the session actor asks the
    // orchestrator what to do at load, and a pending run's answer is its first
    // step. There is no message to queue.
    let id = ask(services, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    // Just created, so it carries no annotations yet.
    let rec = SessionRecord {
        spec,
        created_at,
        annotations: Default::default(),
        status: SessionStatus::Idle,
        forks: Vec::new(),
    };
    Ok(WorkflowRunResponse {
        session: handlers::summary(&id, &rec),
    })
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

    fn operations() -> Vec<Operation> {
        Workflows.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            ["create", "delete", "get", "list", "replace", "run"]
        );
        assert_eq!(Workflows.name(), "workflows");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
