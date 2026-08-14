//! HTTP surface for workflow definitions.
//!
//! Runs are not here: a run is a session, so it is created at
//! `POST /api/workflows/:name/runs` but read, watched, interrupted and deleted
//! through the session API like any other.

use super::Scope;
use super::error::Api;
use super::handlers;
use crate::sessions::builder::build_session_spec;
use crate::sessions::spec::{AgentSettings, SessionOrigin, SessionStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use crate::sessions::workflow::{
    DEFAULT_MAX_STEPS, StepRun, StepStatus, TransitionSpec, WorkflowRunSpec, WorkflowRunState,
    WorkflowStepSpec,
};
use crate::workflows::WorkflowError;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::now_ms;
use horsie_models::session::AgentSettings as WireAgentSettings;
use horsie_models::workflow::{
    RunEdge, RunNode, StepCancelled, StepConcluded, StepFailed, StepRunStatus, StepRunView,
    StepRunning, WorkflowInput, WorkflowRetryRequest, WorkflowRunGraph, WorkflowRunRequest,
    WorkflowRunResponse, WorkflowView,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Seconds since the epoch, the stamp both `agents` and `routines` store.
fn now_secs() -> u64 {
    now_ms() / 1_000
}

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: WorkflowError) -> Api {
    match e {
        WorkflowError::NotFound(m) => Api::not_found(m),
        WorkflowError::Conflict(m) => Api::conflict("conflict", m),
        WorkflowError::Invalid(m) => Api::unprocessable(m),
        WorkflowError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/workflows
pub async fn list_workflows(Scope(state): Scope) -> Result<Json<Vec<WorkflowView>>, Api> {
    state.workflows.list().await.map(Json).map_err(api_err)
}

/// POST /api/workflows
pub async fn create_workflow(
    Scope(state): Scope,
    Json(input): Json<WorkflowInput>,
) -> Result<(StatusCode, Json<WorkflowView>), Api> {
    state
        .workflows
        .create(input, now_secs())
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/workflows/:name
pub async fn get_workflow(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<WorkflowView>, Api> {
    state.workflows.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/workflows/:name — full replace; the path is the id of record.
///
/// A run snapshots the definition when it is created, so editing a workflow
/// never changes a run already under way.
pub async fn replace_workflow(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<WorkflowInput>,
) -> Result<Json<WorkflowView>, Api> {
    state
        .workflows
        .replace(&name, input, now_secs())
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/workflows/:name
///
/// Unlike a routine, this does not delete the workflow's runs: they are
/// sessions in the ordinary session list, each carrying its own snapshot of the
/// graph, and they stay readable afterwards.
pub async fn delete_workflow(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .workflows
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

/// POST /api/workflows/:name/runs — start a run.
///
/// A run is a session, so this takes the configuration creating a session
/// takes: the environment its one shared runtime is built from. Everything the
/// graph decides is snapshotted here, so editing the definition, a preset or
/// the environment afterwards leaves this run alone.
pub async fn start_run(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(req): Json<WorkflowRunRequest>,
) -> Result<(StatusCode, Json<WorkflowRunResponse>), Api> {
    if req.input.trim().is_empty() {
        return Err(Api::unprocessable(
            "input must not be empty — it is what the first step is handed",
        ));
    }
    let row = state.workflows.row(&name).await.map_err(api_err)?;
    let view = state.config_store.view().await.map_err(Api::internal)?;

    // Resolve every step's preset once, here. After this the run is
    // self-contained: the driver never reaches a store, and a preset edited
    // mid-run cannot change a step that has not started yet.
    let mut steps = Vec::with_capacity(row.steps.len());
    let mut plugins: Vec<String> = Vec::new();
    for step in &row.steps {
        let preset = state.agents.get(&step.agent).await.map_err(|_| {
            Api::unprocessable(format!(
                "step '{}': agent preset '{}' no longer exists",
                step.name, step.agent
            ))
        })?;
        if !view.models.iter().any(|m| m.alias == preset.model) {
            return Err(Api::unprocessable(format!(
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
                    condition: t.condition,
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
    // The session's own `agent` settings are the first step's: they are what a
    // session-shaped reader (usage, the detail document) reports. Each step
    // still runs with its own.
    let first = run
        .step(&row.start)
        .ok_or_else(|| Api::unprocessable(format!("start step '{}' is missing", row.start)))?;
    let wire = WireAgentSettings {
        model: first.settings.model.clone(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: None,
        mcp_servers: Some(first.settings.mcp_servers.clone()),
        memory_spaces: Some(first.settings.memory_spaces.clone()),
        thinking_effort: first.settings.thinking_effort.clone(),
        max_concurrent_subagents: None,
        instructions: first.settings.instructions.clone(),
        auto_compact: first.settings.auto_compact,
    };
    let mut spec = build_session_spec(
        &state.config_store,
        &state.environments,
        req.name.or_else(|| Some(name.clone())),
        wire,
        req.environment,
        Some(plugins),
        SessionOrigin::Workflow {
            workflow: name.clone(),
        },
    )
    .await?;
    // Checked on the *resolved* vendor: a named environment carries its own,
    // so there is nothing to check until the environment has been read.
    if !state
        .connected_vendors
        .connected_names()
        .contains(&spec.vendor)
    {
        return Err(Api::unprocessable(format!(
            "runtime vendor '{}' is not connected",
            spec.vendor
        )));
    }
    spec.workflow = Some(run);

    let created_at = now_ms();
    // Creating it is enough to start it: the session actor asks the
    // orchestrator what to do at load, and a pending run's answer is its first
    // step. There is no message to queue.
    let id = handlers::ask(&state, |reply| SessionSupervisorCommand::Create {
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
    Ok((
        StatusCode::CREATED,
        Json(WorkflowRunResponse {
            session: handlers::summary(&id, &rec),
        }),
    ))
}

/// GET /api/sessions/:id/workflow — the run, projected onto its graph.
///
/// Hangs off the session because that is what a run *is*. Every node of the
/// definition is present, including ones the run never reached, so the client
/// draws the whole graph and lights up what happened.
pub async fn get_run_graph(
    Scope(state): Scope,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunGraph>, Api> {
    let (rec, _) = handlers::ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let spec = rec
        .spec
        .workflow
        .clone()
        .ok_or_else(|| Api::not_found("this session is not a workflow run"))?;
    let run = handlers::ask(&state, |reply| SessionSupervisorCommand::RunState {
        id: id.clone(),
        reply,
    })
    .await?
    .unwrap_or_default();
    let usage = handlers::ask(&state, |reply| SessionSupervisorCommand::UsageStats {
        id: id.clone(),
        reply,
    })
    .await?;
    let per_agent = usage.as_ref().map(|s| s.agents.clone()).unwrap_or_default();
    let total = usage.map(|s| s.session_total).unwrap_or_default();
    Ok(Json(project_run(&spec, &run, total, &per_agent)))
}

/// POST /api/sessions/:id/workflow/retry — re-run one execution.
pub async fn retry_step(
    Scope(state): Scope,
    Path(id): Path<String>,
    Json(req): Json<WorkflowRetryRequest>,
) -> Result<StatusCode, Api> {
    handlers::ask(&state, |reply| SessionSupervisorCommand::RetryStep {
        id: id.clone(),
        index: req.step_index,
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?
    .map_err(Api::unprocessable)?;
    Ok(StatusCode::ACCEPTED)
}

/// Project a run's log onto the graph its definition describes.
///
/// Nodes come from the definition, so a step never reached still renders;
/// edges likewise, carrying which executions took them. That is what makes the
/// `Vec` log and the graph view the same information.
fn project_run(
    spec: &WorkflowRunSpec,
    run: &WorkflowRunState,
    usage: crate::agent_loop::UsageTotal,
    per_agent: &HashMap<String, crate::agent_loop::UsageTotal>,
) -> WorkflowRunGraph {
    let nodes = spec
        .steps
        .iter()
        .map(|step| RunNode {
            step: step.name.clone(),
            runs: run
                .steps
                .iter()
                .enumerate()
                .filter(|(_, r)| r.step == step.name)
                .map(|(i, r)| step_run_view(i as u32, r, per_agent))
                .collect(),
        })
        .collect();
    let edges = spec
        .steps
        .iter()
        .flat_map(|step| {
            step.transitions.iter().map(move |t| RunEdge {
                from: step.name.clone(),
                to: t.to.clone(),
                condition: t.condition.clone(),
                traversals: run
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.step == t.to
                            && r.from
                                .and_then(|f| run.get(f))
                                .is_some_and(|src| src.step == step.name)
                            && r.via == t.condition
                    })
                    .map(|(i, _)| i as u32)
                    .collect(),
            })
        })
        .collect();
    WorkflowRunGraph {
        workflow: spec.workflow.clone(),
        current: run.current(),
        start: spec.start.clone(),
        nodes,
        edges,
        output: run.output.clone(),
        error: run.error.clone(),
        input_tokens: usage.input_tokens.try_into().unwrap_or(u32::MAX),
        output_tokens: usage.output_tokens.try_into().unwrap_or(u32::MAX),
    }
}

fn step_run_view(
    index: u32,
    r: &StepRun,
    per_agent: &HashMap<String, crate::agent_loop::UsageTotal>,
) -> StepRunView {
    // A step's agent id is exactly how its usage is banked, so the lookup needs
    // nothing but the map. Zero until the execution's turn ends, which is when
    // usage is recorded.
    let usage = per_agent
        .get(&r.agent.to_string())
        .copied()
        .unwrap_or_default();
    StepRunView {
        index,
        step: r.step.clone(),
        agent_id: r.agent.to_string(),
        attempt: r.attempt,
        status: match r.status {
            StepStatus::Running => StepRunStatus::Running(StepRunning {}),
            StepStatus::Concluded => StepRunStatus::Concluded(StepConcluded {}),
            StepStatus::Failed => StepRunStatus::Failed(StepFailed {}),
            StepStatus::Cancelled => StepRunStatus::Cancelled(StepCancelled {}),
        },
        output: r.output.clone(),
        error: r.error.clone(),
        started_at_ms: r.started_at_ms,
        ended_at_ms: r.ended_at_ms,
        input_tokens: usage.input_tokens.try_into().unwrap_or(u32::MAX),
        output_tokens: usage.output_tokens.try_into().unwrap_or(u32::MAX),
    }
}
