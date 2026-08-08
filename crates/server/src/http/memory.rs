//! HTTP surface for agent memory: CRUD over memory spaces and the memories in
//! them, for the web UI. The agent reaches the same data through
//! `MemoryToolbox`, not through these routes.

use super::Scope;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use horsie_models::memory::{
    MemoryCreateInput, MemorySpaceCreateInput, MemorySpaceUpdateInput, MemorySpaceView,
    MemoryUpdateInput, MemoryView,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    space: Option<String>,
}

/// GET /api/memory-spaces
pub async fn list_spaces(Scope(state): Scope) -> Result<Json<Vec<MemorySpaceView>>, Api> {
    state
        .memory
        .list_spaces()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// POST /api/memory-spaces
pub async fn create_space(
    Scope(state): Scope,
    Json(input): Json<MemorySpaceCreateInput>,
) -> Result<(StatusCode, Json<MemorySpaceView>), Api> {
    state
        .memory
        .create_space(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// PUT /api/memory-spaces/:name — rename and/or re-describe.
pub async fn update_space(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<MemorySpaceUpdateInput>,
) -> Result<Json<MemorySpaceView>, Api> {
    state
        .memory
        .update_space(&name, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/memory-spaces/:name — removes the space and its memories.
pub async fn delete_space(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .memory
        .delete_space(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::not_found)
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
