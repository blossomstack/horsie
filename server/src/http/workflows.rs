//! HTTP surface for workflow definitions.
//!
//! Runs are not here: a run is a session, so it is created at
//! `POST /api/workflows/:name/runs` but read, watched, interrupted and deleted
//! through the session API like any other.

use super::AppState;
use super::error::Api;
use crate::workflows::WorkflowError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::now_ms;
use horsie_models::workflow::{WorkflowInput, WorkflowView};

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
pub async fn list_workflows(State(state): State<AppState>) -> Result<Json<Vec<WorkflowView>>, Api> {
    state.workflows.list().await.map(Json).map_err(api_err)
}

/// POST /api/workflows
pub async fn create_workflow(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<WorkflowView>, Api> {
    state.workflows.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/workflows/:name — full replace; the path is the id of record.
///
/// A run snapshots the definition when it is created, so editing a workflow
/// never changes a run already under way.
pub async fn replace_workflow(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .workflows
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}
