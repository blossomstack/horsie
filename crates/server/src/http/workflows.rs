//! What is left of the workflow HTTP surface: a run projected onto its graph,
//! and retrying a step.
//!
//! The definitions themselves are control-plane operations now. These two are
//! not, because they hang off a *session* rather than a workflow — they move
//! when the session resource does.

use super::Scope;
use super::error::Api;
use super::handlers;
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::workflow::{StepRun, StepStatus, WorkflowRunSpec, WorkflowRunState};
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::workflow::{
    RunEdge, RunNode, StepCancelled, StepConcluded, StepFailed, StepRunStatus, StepRunView,
    StepRunning, WorkflowRetryRequest, WorkflowRunGraph,
};
use std::collections::HashMap;

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
        .workflow_run()
        .cloned()
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
                // Rendered, not stored: the log records the label a reader
                // sees, and the definition holds the filter itself.
                condition: t
                    .when
                    .as_ref()
                    .map(horsie_models::workflow::OutcomeFilter::render),
                traversals: run
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.step == t.to
                            && r.from
                                .and_then(|f| run.get(f))
                                .is_some_and(|src| src.step == step.name)
                            && r.via
                                == t.when
                                    .as_ref()
                                    .map(horsie_models::workflow::OutcomeFilter::render)
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
