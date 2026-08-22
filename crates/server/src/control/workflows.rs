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
use crate::projects::ProjectServices;
use crate::sessions::builder::build_workflow_spec;
use crate::sessions::spec::SessionStatus;
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use crate::sessions::workflow::{ResolveRunError, resolve_run_spec};
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
                "/workflows",
                "Every saved workflow definition.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    Ok::<Vec<WorkflowView>, ControlError>(s.workflows.list().await?)
                },
            ),
            op(
                "get",
                Method::Get,
                "/workflows/{name}",
                "One workflow definition by slug.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    Ok::<WorkflowView, ControlError>(s.workflows.get(&i.name).await?)
                },
            ),
            op(
                "create",
                Method::Post,
                "/workflows",
                "Save a new workflow: a graph of steps, each an agent preset plus a \
             fixed prompt, wired by conditions over the step's output.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: WorkflowInput| async move {
                    Ok::<WorkflowView, ControlError>(s.workflows.create(i, now_secs()).await?)
                },
            )
            .created(),
            op(
                "replace",
                Method::Put,
                "/workflows/{name}",
                "Replace a workflow wholesale. Runs already under way keep the \
             definition they snapshotted, so this never changes one.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: WorkflowInput| async move {
                    let name = i.name.clone();
                    Ok::<WorkflowView, ControlError>(
                        s.workflows.replace(&name, i, now_secs()).await?,
                    )
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/workflows/{name}",
                "Delete a workflow. Its past runs are ordinary sessions and stay \
             readable.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.workflows.delete(&i.name).await?;
                    Ok::<(), ControlError>(())
                },
            )
            .no_content(),
            op(
                "run",
                Method::Post,
                "/workflows/{name}/runs",
                "Start a run: creates the session that drives the graph and returns \
             it immediately. The first step begins on its own.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: RunWorkflow| async move { run_workflow(&s, i).await },
            )
            .created(),
        ]
    }
}

async fn run_workflow(
    services: &ProjectServices,
    input: RunWorkflow,
) -> Result<WorkflowRunResponse, ControlError> {
    let RunWorkflow { name, request: req } = input;
    // Resolve every step's preset once, here. After this the run is
    // self-contained: the driver never reaches a store, and a preset edited
    // mid-run cannot change a step that has not started yet. Shared with the
    // `invoke_workflow` tool, so a run created mid-session is the same run.
    let resolved = resolve_run_spec(services, &name, &req.input)
        .await
        .map_err(|e| match e {
            ResolveRunError::NotFound(m) => ControlError::NotFound(m),
            ResolveRunError::Invalid(m) => ControlError::Invalid(m),
            ResolveRunError::Internal(m) => ControlError::Internal(m),
        })?;
    // No `AgentSettings` is fabricated for the session: a run's agents are its
    // steps, each with its own settings, and nothing session-shaped needs one.
    let run_name = req.name.or_else(|| Some(name.clone()));
    let spec = build_workflow_spec(
        &services.environments,
        req.environment,
        resolved.plugins,
        resolved.run,
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
        name: run_name.clone(),
        created_at,
        message: None,
        reply,
    })
    .await?
    .map_err(super::create_failed)?
    .id;
    // Just created, so it carries no annotations yet.
    let rec = SessionRecord {
        spec,
        name: run_name,
        created_at,
        annotations: Default::default(),
        status: SessionStatus::Idle,
        sub_sessions: Vec::new(),
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
