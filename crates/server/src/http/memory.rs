//! HTTP surface for the individual memories inside a space, for the web UI.
//!
//! Spaces themselves are a control-plane resource (`control::memory_spaces`).
//! These stay here: the agent reaches the same rows through `MemoryToolbox`, so
//! a second agent-facing vocabulary over them would be one to keep in step for
//! no caller that wants it.

use super::Scope;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use horsie_models::memory::{MemoryCreateInput, MemoryUpdateInput, MemoryView};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    space: Option<String>,
}

/// GET /api/memories?space=<name>
pub async fn list_memories(
    Scope(state): Scope,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MemoryView>>, Api> {
    state
        .memory
        .list_memories(q.space.as_deref())
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// GET /api/memories/:id
pub async fn get_memory(Scope(state): Scope, Path(id): Path<i64>) -> Result<Json<MemoryView>, Api> {
    state
        .memory
        .get_memory(id)
        .await
        .map(Json)
        .map_err(Api::not_found)
}

/// POST /api/memories
pub async fn create_memory(
    Scope(state): Scope,
    Json(input): Json<MemoryCreateInput>,
) -> Result<(StatusCode, Json<MemoryView>), Api> {
    state
        .memory
        .create_memory(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// PUT /api/memories/:id
pub async fn update_memory(
    Scope(state): Scope,
    Path(id): Path<i64>,
    Json(input): Json<MemoryUpdateInput>,
) -> Result<Json<MemoryView>, Api> {
    state
        .memory
        .update_memory(id, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/memories/:id
pub async fn delete_memory(Scope(state): Scope, Path(id): Path<i64>) -> Result<StatusCode, Api> {
    state
        .memory
        .delete_memory(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::not_found)
}
