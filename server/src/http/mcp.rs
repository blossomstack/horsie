//! Configured-MCP-server endpoints (`/api/mcp/servers`): CRUD plus a
//! connect/smoke-test. Thin wrappers over [`crate::mcp::McpService`]; bodies are
//! fluorite wire types and views never carry secrets.

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use horsie_models::mcp::{McpConnectResult, McpServerInput, McpServerList, McpServerView};

pub async fn list(State(state): State<AppState>) -> Result<Json<McpServerList>, Api> {
    let servers = state.mcp.list().await.map_err(Api::internal)?;
    Ok(Json(McpServerList { servers }))
}

/// `PUT /api/mcp/servers/:name` — upsert; the path is the id of record, so it
/// overrides any `name` in the body.
pub async fn upsert(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(mut input): Json<McpServerInput>,
) -> Result<Json<McpServerView>, Api> {
    input.name = name;
    state
        .mcp
        .upsert(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

pub async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Result<(), Api> {
    state.mcp.delete(&name).await.map_err(Api::internal)
}

/// `POST /api/mcp/servers/:name/test` — connect (`initialize` + `tools/list`),
/// persist the outcome, and return it. Always `200` with the result envelope;
/// a failed connect is `ok: false` with `error`, not an HTTP error.
pub async fn test(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<McpConnectResult>, Api> {
    state.mcp.test(&name).await.map(Json).map_err(Api::internal)
}
